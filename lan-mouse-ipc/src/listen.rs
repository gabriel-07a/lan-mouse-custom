use futures::{Future, Stream, StreamExt, stream::SelectAll};
#[cfg(unix)]
use std::path::PathBuf;
use std::{
    collections::HashMap,
    io::{self, ErrorKind},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf, ReadHalf, WriteHalf};
use tokio::time::{Duration, timeout};

#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(windows)]
use tokio::net::TcpListener;
#[cfg(windows)]
use tokio::net::TcpStream;

use crate::{FrontendEvent, FrontendRequest, IpcError, IpcListenerCreationError};

/// Maximum number of frontends that may be connected to one service.
///
/// A normal session has one GUI and occasionally a CLI/doctor process. The cap
/// exists so a local process cannot exhaust file descriptors or memory by
/// opening thousands of connections (especially relevant on Windows, where the
/// loopback socket carries no peer credentials - see the README).
pub(crate) const MAX_FRONTENDS: usize = 16;

/// Maximum length of one line-delimited JSON request/event.
///
/// The largest legitimate frame is a full state snapshot of a dozen clients
/// (a few KiB); 1 MiB leaves generous headroom while bounding the memory a
/// misbehaving or hostile peer can make the service allocate for a single
/// frame.
pub(crate) const MAX_MESSAGE_LINE: usize = 1024 * 1024;

/// How long a single broadcast write may take before the frontend is
/// considered stuck and dropped. A healthy local socket drains immediately;
/// the timeout only exists so a frozen GUI cannot stall the service loop.
const BROADCAST_TIMEOUT: Duration = Duration::from_millis(250);

/// Upper bound for shutting down a broken frontend connection.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);

#[cfg(unix)]
type IpcStream = UnixStream;
#[cfg(windows)]
type IpcStream = TcpStream;

/// A line-delimited JSON reader with a hard per-line size limit.
///
/// Replaces `BufReader::lines()`, which grows a `String` without bound until a
/// newline arrives: a single unterminated line could otherwise make the
/// service allocate arbitrarily much memory. Frames that exceed the limit are
/// reported as an error and the caller closes that connection (other frontends
/// are unaffected).
struct LineStream<R> {
    reader: R,
    /// Bytes received from the socket but not yet consumed.
    buffer: Vec<u8>,
    max_len: usize,
    done: bool,
    /// Skipping the remainder of an oversized line.
    discarding: bool,
}

impl<R> LineStream<R> {
    fn new(reader: R, max_len: usize) -> Self {
        Self {
            reader,
            buffer: Vec::new(),
            max_len,
            done: false,
            discarding: false,
        }
    }
}

fn decode_line(bytes: Vec<u8>) -> io::Result<String> {
    String::from_utf8(bytes).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))
}

fn line_too_long(limit: usize) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidData,
        format!("IPC message exceeds the {limit}-byte limit"),
    )
}

impl<R: AsyncRead + Unpin> Stream for LineStream<R> {
    type Item = io::Result<String>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let mut chunk = [0u8; 4096];
        loop {
            if this.done {
                return Poll::Ready(None);
            }

            if this.discarding {
                // Drop everything up to and including the newline that ends
                // the oversized frame; the error was already reported.
                match this.buffer.iter().position(|b| *b == b'\n') {
                    Some(pos) => {
                        this.buffer.drain(..=pos);
                        this.discarding = false;
                    }
                    None => this.buffer.clear(),
                }
            }

            if !this.discarding {
                if let Some(pos) = this.buffer.iter().position(|b| *b == b'\n') {
                    if pos > this.max_len {
                        this.buffer.drain(..=pos);
                        return Poll::Ready(Some(Err(line_too_long(this.max_len))));
                    }
                    let mut line: Vec<u8> = this.buffer.drain(..=pos).collect();
                    // Drop the trailing newline itself.
                    line.pop();
                    return Poll::Ready(Some(decode_line(line)));
                }
                if this.buffer.len() > this.max_len {
                    this.buffer.clear();
                    this.discarding = true;
                    return Poll::Ready(Some(Err(line_too_long(this.max_len))));
                }
            }

            let mut read_buf = ReadBuf::new(&mut chunk);
            match Pin::new(&mut this.reader).poll_read(cx, &mut read_buf) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => {
                    this.done = true;
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(Ok(())) if read_buf.filled().is_empty() => {
                    // EOF: a final line without a trailing newline is still a
                    // line (mirrors `BufRead::lines` behaviour).
                    this.done = true;
                    if this.discarding || this.buffer.is_empty() {
                        return Poll::Ready(None);
                    }
                    let line = std::mem::take(&mut this.buffer);
                    return Poll::Ready(Some(decode_line(line)));
                }
                Poll::Ready(Ok(())) => {
                    this.buffer.extend_from_slice(read_buf.filled());
                }
            }
        }
    }
}

/// One event produced by a frontend connection.
enum ClientEvent {
    Line(String),
    Closed { id: u64, reason: io::Error },
}

/// A connected frontend: read half plus a stable id used to pair it with its
/// write half in `AsyncFrontendListener::tx_streams`.
struct FrontendConnection {
    id: u64,
    lines: LineStream<ReadHalf<IpcStream>>,
    /// Set once the close event was emitted; the next poll returns `None` so
    /// `SelectAll` drops the stream entirely.
    finished: bool,
    /// Set by the writer side when this frontend stopped reading (broadcast
    /// timeout). The read half must then end too: otherwise a peer that keeps
    /// its side open without reading would retain the fd forever and defeat
    /// the connection cap.
    closed: Arc<AtomicBool>,
}

impl Stream for FrontendConnection {
    type Item = ClientEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }
        if self.closed.load(Ordering::Relaxed) {
            self.finished = true;
            return Poll::Ready(None);
        }
        match self.lines.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(line))) => Poll::Ready(Some(ClientEvent::Line(line))),
            Poll::Ready(Some(Err(reason))) => {
                self.finished = true;
                Poll::Ready(Some(ClientEvent::Closed {
                    id: self.id,
                    reason,
                }))
            }
            Poll::Ready(None) => {
                self.finished = true;
                Poll::Ready(Some(ClientEvent::Closed {
                    id: self.id,
                    reason: io::Error::from(ErrorKind::UnexpectedEof),
                }))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub struct AsyncFrontendListener {
    #[cfg(windows)]
    listener: TcpListener,
    #[cfg(unix)]
    listener: UnixListener,
    #[cfg(unix)]
    socket_path: PathBuf,
    connections: SelectAll<FrontendConnection>,
    /// Write halves paired with the connection id.
    tx_streams: Vec<(u64, WriteHalf<IpcStream>)>,
    /// Shared "this frontend is closed" flags, so a broadcast failure ends the
    /// paired read half too (and releases its fd) instead of leaving it behind.
    closed_flags: HashMap<u64, Arc<AtomicBool>>,
    next_id: u64,
    /// One-shot timer used to retry after an accept error at a bounded rate
    /// instead of spinning or stalling until some unrelated event wakes the
    /// service.
    accept_retry: Pin<Box<tokio::time::Sleep>>,
    retry_scheduled: bool,
}

impl AsyncFrontendListener {
    pub async fn new() -> Result<Self, IpcListenerCreationError> {
        #[cfg(unix)]
        {
            let socket_path = crate::default_socket_path()?;
            Self::new_at(socket_path).await
        }

        #[cfg(windows)]
        {
            let listener = match TcpListener::bind("127.0.0.1:5252").await {
                Ok(ls) => ls,
                // some other lan-mouse instance has bound the socket in the meantime
                Err(e) if e.kind() == ErrorKind::AddrInUse => {
                    return Err(IpcListenerCreationError::AlreadyRunning);
                }
                Err(e) => return Err(IpcListenerCreationError::Bind(e)),
            };

            Ok(Self {
                listener,
                connections: SelectAll::new(),
                tx_streams: vec![],
                closed_flags: HashMap::new(),
                next_id: 0,
                accept_retry: Box::pin(tokio::time::sleep(Duration::ZERO)),
                retry_scheduled: false,
            })
        }
    }

    /// Bind the frontend socket at an explicit path (Unix only). Used by
    /// tests and by callers that need an isolated service instance.
    #[cfg(unix)]
    pub async fn new_at(socket_path: PathBuf) -> Result<Self, IpcListenerCreationError> {
        log::debug!("remove socket: {socket_path:?}");
        if socket_path.exists() {
            // try to connect to see if some other instance
            // of lan-mouse is already running
            match UnixStream::connect(&socket_path).await {
                // connected -> lan-mouse is already running
                Ok(_) => return Err(IpcListenerCreationError::AlreadyRunning),
                // lan-mouse is not running but a socket was left behind
                Err(e) if is_stale_socket_error(&e) => {
                    log::debug!("{socket_path:?}: {e} - removing left behind socket");
                    if let Err(e) = std::fs::remove_file(&socket_path) {
                        log::warn!("could not remove stale IPC socket {socket_path:?}: {e}");
                    }
                }
                // Anything else (fd exhaustion, permission problems, ...) is
                // NOT proof of a stale socket: deleting the path here would
                // steal a running service's socket. Fail instead.
                Err(e) => {
                    log::warn!("could not probe existing IPC socket {socket_path:?}: {e}");
                    return Err(IpcListenerCreationError::Probe(e));
                }
            }
        }
        let listener = match UnixListener::bind(&socket_path) {
            Ok(ls) => ls,
            // some other lan-mouse instance has bound the socket in the meantime
            Err(e) if e.kind() == ErrorKind::AddrInUse => {
                return Err(IpcListenerCreationError::AlreadyRunning);
            }
            Err(e) => return Err(IpcListenerCreationError::Bind(e)),
        };

        // Defence in depth: the socket lives in a private 0700 directory and
        // peer credentials are checked on accept, but a permissive
        // XDG_RUNTIME_DIR must not turn the socket itself world-writable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) =
                std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
            {
                log::warn!("could not restrict IPC socket permissions: {e}");
            }
        }

        Ok(Self {
            listener,
            socket_path,
            connections: SelectAll::new(),
            tx_streams: vec![],
            closed_flags: HashMap::new(),
            next_id: 0,
            accept_retry: Box::pin(tokio::time::sleep(Duration::ZERO)),
            retry_scheduled: false,
        })
    }

    /// Number of currently connected frontends (diagnostics and tests).
    pub fn frontend_count(&self) -> usize {
        self.tx_streams.len()
    }

    pub async fn broadcast(&mut self, notify: FrontendEvent) {
        // encode event
        let mut json = match serde_json::to_string(&notify) {
            Ok(json) => json,
            Err(e) => {
                log::error!("could not encode frontend event: {e}");
                return;
            }
        };
        json.push('\n');

        if self.tx_streams.is_empty() {
            return;
        }

        // Write to every frontend concurrently `write_all`-style and bound the
        // whole operation with a timeout, so one stuck GUI cannot freeze the
        // service loop (and a partial write is never followed by another
        // frame, which would corrupt the line protocol).
        let bytes = json.as_bytes();
        let writes = self
            .tx_streams
            .iter_mut()
            .map(|(_, tx)| timeout(BROADCAST_TIMEOUT, tx.write_all(bytes)));
        let results = futures::future::join_all(writes).await;

        let mut keep = Vec::with_capacity(self.tx_streams.len());
        let mut failed = Vec::new();
        for ((id, tx), result) in std::mem::take(&mut self.tx_streams)
            .into_iter()
            .zip(results)
        {
            match result {
                Ok(Ok(())) => keep.push((id, tx)),
                Ok(Err(e)) => {
                    log::debug!("dropping IPC frontend {id}: {e}");
                    failed.push((id, tx));
                }
                Err(_) => {
                    log::warn!("IPC frontend {id} stopped reading; dropping it");
                    failed.push((id, tx));
                }
            }
        }
        self.tx_streams = keep;
        for (id, mut tx) in failed {
            // Closing the write side makes a peer that is still reading see a
            // clean EOF instead of a half-written frame. The read half is
            // signalled too: a peer that keeps its side open without reading
            // would otherwise retain the fd forever (defeating the cap).
            if let Some(closed) = self.closed_flags.get(&id) {
                closed.store(true, Ordering::Relaxed);
            }
            let _ = timeout(SHUTDOWN_TIMEOUT, tx.shutdown()).await;
            log::debug!("closed IPC frontend {id}");
        }
    }
}

/// Whether `connect` failing on an existing path means "stale socket left
/// behind by a crashed service" rather than "a live service we cannot reach".
#[cfg(unix)]
fn is_stale_socket_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::NotFound
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
    )
}

#[cfg(unix)]
impl Drop for AsyncFrontendListener {
    fn drop(&mut self) {
        log::debug!("remove socket: {:?}", self.socket_path);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Whether the IPC client runs as the same user as the service.
#[cfg(unix)]
fn peer_is_same_user(stream: &UnixStream) -> bool {
    match stream.peer_cred() {
        Ok(cred) => cred.uid() == unsafe { libc::geteuid() },
        Err(e) => {
            log::warn!("could not read IPC peer credentials: {e}");
            false
        }
    }
}

impl Stream for AsyncFrontendListener {
    type Item = Result<FrontendRequest, IpcError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.connections.poll_next_unpin(cx) {
                Poll::Ready(Some(ClientEvent::Line(line))) => {
                    // A frame this build cannot decode is skipped (not fatal):
                    // that is how a frontend and a service from different
                    // versions stay usable together.
                    let request = serde_json::from_str(line.as_str()).map_err(IpcError::from);
                    return Poll::Ready(Some(request));
                }
                Poll::Ready(Some(ClientEvent::Closed { id, reason })) => {
                    log::debug!("IPC frontend {id} disconnected: {reason}");
                    self.tx_streams.retain(|(tx_id, _)| *tx_id != id);
                    self.closed_flags.remove(&id);
                    continue;
                }
                Poll::Ready(None) | Poll::Pending => break,
            }
        }
        let mut accepted = false;
        loop {
            match self.listener.poll_accept(cx) {
                Poll::Ready(Ok((stream, _))) => {
                    self.retry_scheduled = false;
                    if self.tx_streams.len() >= MAX_FRONTENDS {
                        log::warn!(
                            "rejecting IPC connection: already {MAX_FRONTENDS} frontends connected"
                        );
                        drop(stream);
                        continue;
                    }
                    // On Unix the local socket lives in $XDG_RUNTIME_DIR, but
                    // the permission check is enforced explicitly so another
                    // local user can never drive this service (which can grant
                    // control over the machine). Windows uses a loopback TCP
                    // socket, which cannot carry peer credentials; that
                    // limitation is documented in the README.
                    #[cfg(unix)]
                    if !peer_is_same_user(&stream) {
                        log::warn!("rejecting IPC connection from a different user");
                        continue;
                    }
                    self.next_id = self.next_id.wrapping_add(1);
                    let id = self.next_id;
                    let (rx, tx) = tokio::io::split(stream);
                    let closed = Arc::new(AtomicBool::new(false));
                    self.connections.push(FrontendConnection {
                        id,
                        lines: LineStream::new(rx, MAX_MESSAGE_LINE),
                        finished: false,
                        closed: closed.clone(),
                    });
                    self.closed_flags.insert(id, closed);
                    self.tx_streams.push((id, tx));
                    accepted = true;
                }
                Poll::Ready(Err(e)) => {
                    log::warn!("failed to accept IPC connection: {e}");
                    // Transient errors (ECONNABORTED) are retried promptly;
                    // persistent ones (e.g. fd exhaustion) are retried at a
                    // bounded rate instead of spinning at 100% CPU.
                    if !self.retry_scheduled {
                        self.retry_scheduled = true;
                        self.accept_retry
                            .as_mut()
                            .reset(tokio::time::Instant::now() + Duration::from_millis(100));
                    }
                    break;
                }
                Poll::Pending => break,
            }
        }
        if self.retry_scheduled && self.accept_retry.as_mut().poll(cx).is_ready() {
            // Deadline elapsed: try to accept again.
            self.retry_scheduled = false;
            cx.waker().wake_by_ref();
        }
        if accepted {
            // A freshly connected frontend must receive the full state; the
            // service answers this synthetic request with a full sync.
            Poll::Ready(Some(Ok(FrontendRequest::Sync)))
        } else {
            Poll::Pending
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn stale_socket_errors_are_recognised() {
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::ConnectionRefused,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
        ] {
            assert!(
                is_stale_socket_error(&io::Error::from(kind)),
                "{kind:?} must mean a stale socket file"
            );
        }
        // These must never cause the socket file of a possibly running
        // service to be deleted.
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::OutOfMemory,
            ErrorKind::WouldBlock,
            ErrorKind::TimedOut,
        ] {
            assert!(
                !is_stale_socket_error(&io::Error::from(kind)),
                "{kind:?} must not be treated as a stale socket"
            );
        }
    }
}
