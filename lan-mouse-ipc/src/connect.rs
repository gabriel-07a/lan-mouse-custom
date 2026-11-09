use crate::{ConnectionError, FrontendEvent, FrontendRequest, IpcError};
use std::{
    cmp::min,
    io::{self, BufReader, LineWriter, Lines, prelude::*},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

#[cfg(windows)]
use std::net::TcpStream;

/// How long `connect()` waits for the service to come up before giving up.
///
/// The frontends start the service themselves, so a short wait is expected;
/// waiting forever (as an unbounded retry loop does) silently hangs the GUI
/// when the service cannot start at all (port taken, broken config, crash
/// loop). 30 s is long enough for a slow first start and short enough to
/// surface a real failure.
const SERVICE_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct FrontendEventReader {
    #[cfg(unix)]
    lines: Lines<BufReader<UnixStream>>,
    #[cfg(windows)]
    lines: Lines<BufReader<TcpStream>>,
}

pub struct FrontendRequestWriter {
    #[cfg(unix)]
    line_writer: LineWriter<UnixStream>,
    #[cfg(windows)]
    line_writer: LineWriter<TcpStream>,
}

impl FrontendEventReader {
    pub fn next_event(&mut self) -> Option<Result<FrontendEvent, IpcError>> {
        loop {
            match self.lines.next()? {
                Err(e) => return Some(Err(e.into())),
                Ok(line) => match serde_json::from_str(line.as_str()) {
                    Ok(event) => return Some(Ok(event)),
                    // A newer daemon may know event variants this frontend
                    // does not. Skipping the frame keeps an old GUI/CLI alive
                    // instead of killing it on the first unknown event.
                    Err(e) => log::warn!("skipping undecodable frontend event: {e}"),
                },
            }
        }
    }
}

impl FrontendRequestWriter {
    pub fn request(&mut self, request: FrontendRequest) -> Result<(), io::Error> {
        let mut json = serde_json::to_string(&request)?;
        log::debug!("requesting: {json}");
        json.push('\n');
        self.line_writer.write_all(json.as_bytes())?;
        Ok(())
    }
}

/// Try to connect to the service once without retrying.
pub fn try_connect() -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    #[cfg(unix)]
    let rx = {
        let socket_path = crate::default_socket_path()?;
        UnixStream::connect(&socket_path)?
    };
    #[cfg(windows)]
    let rx = TcpStream::connect("127.0.0.1:5252")?;
    make_connection(rx)
}

pub fn connect() -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let rx = wait_for_service()?;
    make_connection(rx)
}

/// Retry `attempt` with capped exponential backoff until it succeeds or
/// `timeout` elapses, then report [`ConnectionError::Timeout`].
fn retry_until<T>(
    mut attempt: impl FnMut() -> io::Result<T>,
    timeout: Duration,
) -> Result<T, ConnectionError> {
    let deadline = Instant::now() + timeout;
    let mut duration = Duration::from_millis(10);
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(e) => {
                // "Service not up yet" is retried; a permission/argument
                // problem is not - waiting 30 s for it would only obscure the
                // real error.
                if !is_retryable_connect_error(&e) {
                    return Err(ConnectionError::Io(e));
                }
                let now = Instant::now();
                if now >= deadline {
                    return Err(ConnectionError::Timeout);
                }
                let sleep = exponential_back_off(&mut duration).min(deadline - now);
                thread::sleep(sleep);
            }
        }
    }
}

/// Errors that can legitimately mean "the service is not listening yet".
fn is_retryable_connect_error(error: &io::Error) -> bool {
    !matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied
            | io::ErrorKind::InvalidInput
            | io::ErrorKind::NotADirectory
            | io::ErrorKind::IsADirectory
    )
}

#[cfg(unix)]
fn make_connection(
    rx: UnixStream,
) -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let tx = rx.try_clone()?;
    let buf_reader = BufReader::new(rx);
    let lines = buf_reader.lines();
    let line_writer = LineWriter::new(tx);
    let reader = FrontendEventReader { lines };
    let writer = FrontendRequestWriter { line_writer };
    Ok((reader, writer))
}

#[cfg(windows)]
fn make_connection(
    rx: TcpStream,
) -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let tx = rx.try_clone()?;
    let buf_reader = BufReader::new(rx);
    let lines = buf_reader.lines();
    let line_writer = LineWriter::new(tx);
    let reader = FrontendEventReader { lines };
    let writer = FrontendRequestWriter { line_writer };
    Ok((reader, writer))
}

/// wait for the lan-mouse socket to come online
#[cfg(unix)]
fn wait_for_service() -> Result<UnixStream, ConnectionError> {
    let socket_path = crate::default_socket_path()?;
    // a signaling mechanism or inotify could be used to improve this
    retry_until(
        || UnixStream::connect(&socket_path),
        SERVICE_CONNECT_TIMEOUT,
    )
}

#[cfg(windows)]
fn wait_for_service() -> Result<TcpStream, ConnectionError> {
    retry_until(
        || TcpStream::connect("127.0.0.1:5252"),
        SERVICE_CONNECT_TIMEOUT,
    )
}

fn exponential_back_off(duration: &mut Duration) -> Duration {
    let new = duration.saturating_mul(2);
    *duration = min(new, Duration::from_secs(1));
    *duration
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_until_returns_the_first_success() {
        let mut calls = 0;
        let value = retry_until(
            || {
                calls += 1;
                if calls < 3 {
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused))
                } else {
                    Ok(42)
                }
            },
            Duration::from_secs(2),
        )
        .expect("retry must succeed");
        assert_eq!(value, 42);
        assert_eq!(calls, 3);
    }

    #[test]
    fn retry_until_gives_up_with_timeout_instead_of_hanging() {
        let start = Instant::now();
        let result = retry_until(
            || Err(io::Error::from(io::ErrorKind::ConnectionRefused)) as io::Result<()>,
            Duration::from_millis(60),
        );
        assert!(matches!(result, Err(ConnectionError::Timeout)));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn blocking_reader_skips_unknown_event_variants() {
        // Forward compatibility: a service from a newer version may send a
        // variant this frontend does not know. The reader must skip it and
        // deliver the next frame instead of failing/blocking.
        use std::os::unix::net::UnixStream;

        let (mut service, frontend) = UnixStream::pair().expect("socket pair");
        let mut reader = FrontendEventReader {
            lines: BufReader::new(frontend).lines(),
        };
        service
            .write_all(
                b"{\"SomeFutureEvent\":{\"unknown\":true}}\n\
                  {\"PortChanged\":[4242,null]}\n",
            )
            .expect("write frames");
        drop(service);

        let event = reader
            .next_event()
            .expect("reader ended before the known event")
            .expect("known event must decode");
        assert!(matches!(event, FrontendEvent::PortChanged(4242, None)));
        assert!(reader.next_event().is_none(), "EOF must end the reader");
    }

    #[test]
    fn non_retryable_connect_errors_propagate_immediately() {
        let start = Instant::now();
        let result: Result<(), ConnectionError> = retry_until(
            || Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            Duration::from_secs(10),
        );
        match result {
            Err(ConnectionError::Io(e)) => {
                assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
            }
            other => panic!("expected the permission error to propagate, got {other:?}"),
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "a non-retryable error must not wait for the timeout"
        );
    }

    #[test]
    fn backoff_is_capped_at_one_second() {
        let mut duration = Duration::from_millis(10);
        for _ in 0..20 {
            exponential_back_off(&mut duration);
        }
        assert_eq!(duration, Duration::from_secs(1));
    }
}
