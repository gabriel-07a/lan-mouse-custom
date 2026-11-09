use crate::{ConnectionError, FrontendEvent, FrontendRequest, IpcError};
use std::{
    cmp::min,
    task::{Poll, ready},
    time::Duration,
};

use futures::{Stream, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio_stream::wrappers::LinesStream;

#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(windows)]
use tokio::net::TcpStream;

pub struct AsyncFrontendEventReader {
    #[cfg(unix)]
    lines_stream: LinesStream<BufReader<ReadHalf<UnixStream>>>,
    #[cfg(windows)]
    lines_stream: LinesStream<BufReader<ReadHalf<TcpStream>>>,
}

pub struct AsyncFrontendRequestWriter {
    #[cfg(unix)]
    tx: WriteHalf<UnixStream>,
    #[cfg(windows)]
    tx: WriteHalf<TcpStream>,
}

impl AsyncFrontendEventReader {
    /// Read the next event, optionally giving up after `timeout`.
    ///
    /// Returns `Ok(None)` when the timeout elapses (not an error: the caller
    /// decides whether that is a failure), and errors only for I/O problems.
    /// Frames this build cannot decode are skipped with a warning, exactly
    /// like the blocking [`crate::FrontendEventReader`], so a frontend keeps
    /// working against a newer service.
    pub async fn next_event(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Option<FrontendEvent>, IpcError> {
        let next = async {
            match StreamExt::next(self).await {
                Some(Ok(event)) => Ok(Some(event)),
                Some(Err(e)) => Err(e),
                None => Err(IpcError::Io(std::io::Error::from(
                    std::io::ErrorKind::UnexpectedEof,
                ))),
            }
        };
        match timeout {
            Some(duration) => match tokio::time::timeout(duration, next).await {
                Ok(result) => result,
                Err(_) => Ok(None),
            },
            None => next.await,
        }
    }
}

impl Stream for AsyncFrontendEventReader {
    type Item = Result<FrontendEvent, IpcError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            match ready!(self.lines_stream.poll_next_unpin(cx)) {
                None => return Poll::Ready(None),
                Some(Ok(line)) => match serde_json::from_str(line.as_str()) {
                    Ok(event) => return Poll::Ready(Some(Ok(event))),
                    // A newer service may know event variants this build does
                    // not. Skipping the frame is what makes the IPC forward
                    // compatible; aborting here would turn a display-only
                    // change into a broken frontend/service pair.
                    Err(e) => log::warn!("skipping undecodable frontend event: {e}"),
                },
                Some(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
            }
        }
    }
}

impl AsyncFrontendRequestWriter {
    pub async fn request(&mut self, request: FrontendRequest) -> Result<(), IpcError> {
        let mut json = serde_json::to_string(&request)?;
        log::debug!("requesting: {json}");
        json.push('\n');
        self.tx.write_all(json.as_bytes()).await?;
        Ok(())
    }
}

pub async fn connect_async(
    timeout: Option<Duration>,
) -> Result<(AsyncFrontendEventReader, AsyncFrontendRequestWriter), ConnectionError> {
    #[cfg(unix)]
    {
        let socket_path = crate::default_socket_path()?;
        connect_async_at(&socket_path, timeout).await
    }

    #[cfg(windows)]
    {
        let stream = if let Some(duration) = timeout {
            tokio::select! {
                s = wait_for_service() => s?,
                _ = tokio::time::sleep(duration) => return Err(ConnectionError::Timeout),
            }
        } else {
            wait_for_service().await?
        };
        Ok(split_stream(stream))
    }
}

/// Connect to the frontend socket at an explicit path (Unix only). Used by
/// tests and by callers that need an isolated service instance.
#[cfg(unix)]
pub async fn connect_async_at(
    socket_path: &std::path::Path,
    timeout: Option<Duration>,
) -> Result<(AsyncFrontendEventReader, AsyncFrontendRequestWriter), ConnectionError> {
    let stream = if let Some(duration) = timeout {
        tokio::select! {
            s = wait_for_service_at(socket_path) => s?,
            _ = tokio::time::sleep(duration) => return Err(ConnectionError::Timeout),
        }
    } else {
        wait_for_service_at(socket_path).await?
    };
    Ok(split_stream(stream))
}

#[cfg(unix)]
fn split_stream(stream: UnixStream) -> (AsyncFrontendEventReader, AsyncFrontendRequestWriter) {
    let (rx, tx): (ReadHalf<UnixStream>, WriteHalf<UnixStream>) = tokio::io::split(stream);
    let buf_reader = BufReader::new(rx);
    let lines = buf_reader.lines();
    let lines_stream = LinesStream::new(lines);
    let reader = AsyncFrontendEventReader { lines_stream };
    let writer = AsyncFrontendRequestWriter { tx };
    (reader, writer)
}

#[cfg(windows)]
fn split_stream(stream: TcpStream) -> (AsyncFrontendEventReader, AsyncFrontendRequestWriter) {
    let (rx, tx): (ReadHalf<TcpStream>, WriteHalf<TcpStream>) = tokio::io::split(stream);
    let buf_reader = BufReader::new(rx);
    let lines = buf_reader.lines();
    let lines_stream = LinesStream::new(lines);
    let reader = AsyncFrontendEventReader { lines_stream };
    let writer = AsyncFrontendRequestWriter { tx };
    (reader, writer)
}

/// wait for the lan-mouse socket to come online
#[cfg(unix)]
async fn wait_for_service_at(socket_path: &std::path::Path) -> Result<UnixStream, ConnectionError> {
    let mut duration = Duration::from_millis(10);
    loop {
        if let Ok(stream) = UnixStream::connect(socket_path).await {
            break Ok(stream);
        }
        // a signaling mechanism or inotify could be used to
        // improve this
        tokio::time::sleep(exponential_back_off(&mut duration)).await;
    }
}

#[cfg(windows)]
async fn wait_for_service() -> Result<TcpStream, ConnectionError> {
    let mut duration = Duration::from_millis(10);
    loop {
        if let Ok(stream) = TcpStream::connect("127.0.0.1:5252").await {
            break Ok(stream);
        }
        tokio::time::sleep(exponential_back_off(&mut duration)).await;
    }
}

fn exponential_back_off(duration: &mut Duration) -> Duration {
    let new = duration.saturating_mul(2);
    *duration = min(new, Duration::from_secs(1));
    *duration
}
