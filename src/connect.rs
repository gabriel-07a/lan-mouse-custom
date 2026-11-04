use crate::client::ClientManager;
use crate::config::local_commit;
use lan_mouse_ipc::{ClientHandle, DEFAULT_PORT};
use lan_mouse_proto::{
    MAX_EVENT_SIZE, ProtoEvent,
    clipboard::{ClipboardFrame, MAX_CLIPBOARD_FRAME, is_clipboard_frame},
};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    sync::watch,
    task::{JoinSet, spawn_local},
};
use webrtc_dtls::{
    config::{Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::Conn;

#[derive(Debug, Error)]
pub(crate) enum LanMouseConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
    #[error(transparent)]
    Webrtc(#[from] webrtc_util::Error),
    #[error("not connected")]
    NotConnected,
    #[error("emulation is disabled on the target device")]
    TargetEmulationDisabled,
    #[error("Connection timed out")]
    Timeout,
}

/// Per-address DTLS handshake budget. Addresses are dialed in parallel, so
/// the total time to establish a connection stays close to this value even
/// when a client has several (possibly stale) IPs configured.
const ADDRESS_TIMEOUT: Duration = Duration::from_secs(2);

/// Backoff for the background reconnect loop while a client stays active.
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_millis(250);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);

type ArcConn = Arc<dyn Conn + Send + Sync>;
type ConnMap = Rc<RefCell<HashMap<SocketAddr, ArcConn>>>;
type FastConnMap = Rc<RefCell<HashMap<ClientHandle, ArcConn>>>;

/// Shared state of the outgoing connection layer.
///
/// Everything in this module runs on the single-threaded [`tokio::task::LocalSet`],
/// so interior mutability via `Rc<RefCell<_>>` is enough and keeps the hot
/// send path free of async locks.
#[derive(Clone)]
struct ConnCtx {
    cert: Certificate,
    client_manager: ClientManager,
    /// connections keyed by peer address (source of truth)
    conns: ConnMap,
    /// connections keyed by client handle (hot-path cache)
    fast_conns: FastConnMap,
    /// handles with a connect attempt in flight
    connecting: Rc<RefCell<HashSet<ClientHandle>>>,
    /// handles with a background reconnect loop running
    reconnecting: Rc<RefCell<HashSet<ClientHandle>>>,
    /// broadcasts connection state changes to `send` callers
    waiters: Rc<RefCell<HashMap<ClientHandle, watch::Sender<bool>>>>,
    /// receiver for the upper layer (Ack/Leave)
    tx: Sender<(ClientHandle, ProtoEvent)>,
    /// receiver for clipboard frames arriving on outgoing connections
    clipboard_tx: Sender<(ClientHandle, ClipboardFrame)>,
    /// addresses that answered the current ping round
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    /// timestamp of the last ping sent, used to derive RTT from `Pong`
    ping_sent: Rc<RefCell<HashMap<SocketAddr, Instant>>>,
    /// addresses that already received our build `Hello`. A `Hello` is only
    /// sent once the peer proved it understands the extension by answering a
    /// `Ping` with capabilities: older releases close the connection when
    /// they receive an event id they do not know.
    hello_sent: Rc<RefCell<HashSet<SocketAddr>>>,
    /// how long a single `send` blocks waiting for a connection
    grace: Duration,
}

impl ConnCtx {
    fn notify_connected(&self, handle: ClientHandle, connected: bool) {
        if let Some(tx) = self.waiters.borrow().get(&handle) {
            tx.send_replace(connected);
        }
    }

    fn waiter(&self, handle: ClientHandle) -> watch::Receiver<bool> {
        let mut waiters = self.waiters.borrow_mut();
        if let Some(tx) = waiters.get(&handle) {
            return tx.subscribe();
        }
        let connected = self.client_manager.active_addr(handle).is_some();
        let (tx, rx) = watch::channel(connected);
        waiters.insert(handle, tx);
        rx
    }

    /// Start a background connect attempt, unless one is already running or
    /// the client is already connected.
    fn start_connecting(&self, handle: ClientHandle) {
        if self.client_manager.active_addr(handle).is_some() {
            return;
        }
        if !self.connecting.borrow_mut().insert(handle) {
            return;
        }
        let ctx = self.clone();
        spawn_local(async move {
            if let Err(e) = connect_to_handle(&ctx, handle).await {
                log::debug!("client {handle} connect attempt failed: {e}");
            }
        });
    }

    /// Keep trying to restore a connection for an active client.
    fn schedule_reconnect(&self, handle: ClientHandle) {
        if !self.client_manager.is_active(handle) {
            return;
        }
        if !self.reconnecting.borrow_mut().insert(handle) {
            return;
        }
        let ctx = self.clone();
        spawn_local(async move {
            let mut delay = RECONNECT_BACKOFF_MIN;
            loop {
                tokio::time::sleep(delay).await;
                if !ctx.client_manager.is_active(handle) {
                    break;
                }
                if ctx.client_manager.active_addr(handle).is_some() {
                    break;
                }
                ctx.start_connecting(handle);
                delay = (delay * 2).min(RECONNECT_BACKOFF_MAX);
            }
            ctx.reconnecting.borrow_mut().remove(&handle);
        });
    }

    fn fast_conn(&self, handle: ClientHandle, addr: SocketAddr) -> Option<ArcConn> {
        if let Some(conn) = self.fast_conns.borrow().get(&handle).cloned() {
            return Some(conn);
        }
        let conn = self.conns.borrow().get(&addr).cloned()?;
        self.fast_conns.borrow_mut().insert(handle, conn.clone());
        Some(conn)
    }

    fn disconnect(&self, handle: ClientHandle, addr: SocketAddr) {
        log::warn!("client ({handle}) @ {addr} connection closed");
        self.conns.borrow_mut().remove(&addr);
        self.fast_conns.borrow_mut().remove(&handle);
        self.client_manager.set_active_addr(handle, None);
        self.client_manager.set_alive(handle, false);
        self.client_manager.set_peer_hello(handle, None, 0, 0);
        self.client_manager.set_rtt_ms(handle, None);
        self.ping_sent.borrow_mut().remove(&addr);
        self.ping_response.borrow_mut().remove(&addr);
        self.notify_connected(handle, false);
        self.schedule_reconnect(handle);
    }

    /// Resolve a usable connection for `handle`, waiting up to `remaining`
    /// for a connection attempt that is already (or will be) in flight.
    async fn conn_for(
        &self,
        handle: ClientHandle,
        remaining: Duration,
    ) -> Result<(SocketAddr, ArcConn), LanMouseConnectionError> {
        if let Some(addr) = self.client_manager.active_addr(handle) {
            if let Some(conn) = self.fast_conn(handle, addr) {
                return Ok((addr, conn));
            }
        }

        self.start_connecting(handle);

        if !remaining.is_zero() && !*self.waiter(handle).borrow() {
            let mut rx = self.waiter(handle);
            let _ = tokio::time::timeout(remaining, async {
                while !*rx.borrow() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
        }

        let addr = self
            .client_manager
            .active_addr(handle)
            .ok_or(LanMouseConnectionError::NotConnected)?;
        let conn = self
            .fast_conn(handle, addr)
            .ok_or(LanMouseConnectionError::NotConnected)?;
        Ok((addr, conn))
    }
}

/// Cloneable send half of the connection layer.
///
/// The input capture task owns the receive half; the clipboard task uses this
/// handle to push frames to the same peers.
#[derive(Clone)]
pub(crate) struct ConnectionSender {
    ctx: ConnCtx,
}

pub(crate) struct LanMouseConnection {
    sender: ConnectionSender,
    recv_rx: Receiver<(ClientHandle, ProtoEvent)>,
    clipboard_rx: Option<Receiver<(ClientHandle, ClipboardFrame)>>,
}

impl LanMouseConnection {
    pub(crate) fn new(
        cert: Certificate,
        client_manager: ClientManager,
        connect_grace_ms: u64,
    ) -> Self {
        let (recv_tx, recv_rx) = channel();
        let (clipboard_tx, clipboard_rx) = channel();
        let ctx = ConnCtx {
            cert,
            client_manager,
            conns: Default::default(),
            fast_conns: Default::default(),
            connecting: Default::default(),
            reconnecting: Default::default(),
            waiters: Default::default(),
            tx: recv_tx,
            clipboard_tx,
            ping_response: Default::default(),
            ping_sent: Default::default(),
            hello_sent: Default::default(),
            grace: Duration::from_millis(connect_grace_ms),
        };
        Self {
            sender: ConnectionSender { ctx },
            recv_rx,
            clipboard_rx: Some(clipboard_rx),
        }
    }

    /// Cloneable send half, used by the clipboard sync task.
    pub(crate) fn sender(&self) -> ConnectionSender {
        self.sender.clone()
    }

    /// Hand the clipboard frame receiver to the clipboard task. The input
    /// capture task keeps the input event receiver.
    pub(crate) fn take_clipboard_rx(&mut self) -> Option<Receiver<(ClientHandle, ClipboardFrame)>> {
        self.clipboard_rx.take()
    }

    pub(crate) async fn recv(&mut self) -> (ClientHandle, ProtoEvent) {
        self.recv_rx.recv().await.expect("channel closed")
    }

    /// Pre-establish the connection for an active client so that the first
    /// time the user crosses the screen edge the input can flow immediately.
    pub(crate) fn ensure_connected(&self, handle: ClientHandle) {
        self.sender.ctx.start_connecting(handle);
    }

    /// Whether the peer advertised that it acknowledges `Enter` serials.
    /// Legacy peers (including every upstream release) never acknowledge an
    /// `Enter`, so the capture must use fire-and-forget handoff for them.
    pub(crate) fn peer_supports_enter_ack(&self, handle: ClientHandle) -> bool {
        self.sender.peer_supports_enter_ack(handle)
    }

    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        self.sender.send(event, handle).await
    }

    /// Best-effort send that never waits for a connection. Used by teardown
    /// paths (key-up synthesis, modifier reset, `Leave`) so releasing a
    /// connection to a dead peer does not keep the input grabbed.
    pub(crate) async fn send_now(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        self.sender.send_now(event, handle).await
    }
}

impl ConnectionSender {
    /// Pre-establish the connection for a peer (used when a clipboard peer
    /// is registered before its first frame).
    pub(crate) fn ensure_connected(&self, handle: ClientHandle) {
        self.ctx.start_connecting(handle);
    }

    /// Whether the peer advertised that it acknowledges `Enter` serials.
    /// Legacy peers (including every upstream release) do not, so the
    /// caller must fall back to fire-and-forget handoff.
    pub(crate) fn peer_supports_enter_ack(&self, handle: ClientHandle) -> bool {
        let Some(caps) = self.ctx.client_manager.peer_capabilities(handle) else {
            // No Hello yet: assume a legacy peer and never block on an Ack.
            return false;
        };
        caps & lan_mouse_proto::capabilities::ENTER_ACK != 0
    }

    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        self.send_bytes(&buf[..len], handle, self.ctx.grace, true)
            .await
    }

    pub(crate) async fn send_now(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        self.send_bytes(&buf[..len], handle, Duration::ZERO, true)
            .await
    }

    /// Send an already encoded clipboard frame. Clipboard traffic does not
    /// depend on the peer's input-emulation state, so it skips the `alive`
    /// requirement, and it never blocks on a reconnect (the transfer layer
    /// retransmits instead).
    pub(crate) async fn send_raw(
        &self,
        handle: ClientHandle,
        bytes: &[u8],
    ) -> Result<(), LanMouseConnectionError> {
        self.send_bytes(bytes, handle, Duration::ZERO, false).await
    }

    async fn send_bytes(
        &self,
        buf: &[u8],
        handle: ClientHandle,
        grace: Duration,
        require_alive: bool,
    ) -> Result<(), LanMouseConnectionError> {
        let deadline = Instant::now() + grace;
        let mut retried = false;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (addr, conn) = self.ctx.conn_for(handle, remaining).await?;

            if require_alive && !self.ctx.client_manager.alive(handle) {
                return Err(LanMouseConnectionError::TargetEmulationDisabled);
            }

            match conn.send(buf).await {
                Ok(_) => {
                    log::trace!("{} bytes >->->->->- {addr}", buf.len());
                    return Ok(());
                }
                Err(e) => {
                    log::warn!("client {handle} failed to send: {e}");
                    self.ctx.disconnect(handle, addr);
                    // One transparent retry lets a transient send error be
                    // absorbed without releasing the capture. The retry path
                    // still has to complete before the caller's grace expires.
                    if retried || Instant::now() >= deadline {
                        return Err(e.into());
                    }
                    retried = true;
                }
            }
        }
    }
}

/// Mark a UDP socket for low latency.
///
/// `IP_TOS`/`IPV6_TCLASS` with DSCP EF (46) puts the packets in the WMM
/// voice queue on Wi-Fi and in the priority queue on routers that honour
/// DiffServ, which cuts bufferbloat jitter when the network is busy.
pub(crate) fn configure_low_latency_udp(socket: &UdpSocket) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = socket.as_raw_fd();
        let tos: libc::c_int = 0xB8; // DSCP EF (46) << 2
        unsafe {
            let _ = libc::setsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_TOS,
                &tos as *const _ as *const libc::c_void,
                std::mem::size_of_val(&tos) as libc::socklen_t,
            );
            let _ = libc::setsockopt(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_TCLASS,
                &tos as *const _ as *const libc::c_void,
                std::mem::size_of_val(&tos) as libc::socklen_t,
            );
        }
    }
    #[cfg(not(unix))]
    let _ = socket;
}

async fn connect(
    addr: SocketAddr,
    cert: Certificate,
    timeout: Duration,
) -> Result<(ArcConn, SocketAddr), (SocketAddr, LanMouseConnectionError)> {
    log::info!("connecting to {addr} ...");
    let bind_addr = if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let conn = Arc::new(
        UdpSocket::bind(bind_addr)
            .await
            .map_err(|e| (addr, e.into()))?,
    );
    configure_low_latency_udp(&conn);
    conn.connect(addr).await.map_err(|e| (addr, e.into()))?;
    let config = Config {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    let timeout = tokio::time::sleep(timeout);
    tokio::select! {
        _ = timeout => Err((addr, LanMouseConnectionError::Timeout)),
        result = DTLSConn::new(conn, config, true, None) => match result {
            Ok(dtls_conn) => Ok((Arc::new(dtls_conn), addr)),
            Err(e) => Err((addr, e.into())),
        }
    }
}

async fn connect_any(
    addrs: &[SocketAddr],
    cert: Certificate,
    timeout: Duration,
) -> Result<(ArcConn, SocketAddr), LanMouseConnectionError> {
    if addrs.is_empty() {
        return Err(LanMouseConnectionError::NotConnected);
    }
    let mut joinset = JoinSet::new();
    for &addr in addrs {
        joinset.spawn_local(connect(addr, cert.clone(), timeout));
    }
    while let Some(result) = joinset.join_next().await {
        match result {
            Ok(Ok(conn)) => {
                // Stop the other in-flight attempts: the first successful
                // handshake wins (happy-eyeballs style).
                joinset.abort_all();
                return Ok(conn);
            }
            Ok(Err((addr, e))) => log::warn!("failed to connect to {addr}: `{e}`"),
            Err(e) => log::warn!("connection attempt task failed: {e}"),
        }
    }
    Err(LanMouseConnectionError::NotConnected)
}

async fn connect_to_handle(
    ctx: &ConnCtx,
    handle: ClientHandle,
) -> Result<(), LanMouseConnectionError> {
    log::info!("client {handle} connecting ...");

    let result = async {
        if ctx.client_manager.active_addr(handle).is_some() {
            return Ok(());
        }

        let ips = ctx
            .client_manager
            .get_ips(handle)
            .ok_or(LanMouseConnectionError::NotConnected)?;
        let port = ctx.client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let addrs = ips
            .into_iter()
            .map(|a| SocketAddr::new(a, port))
            .collect::<Vec<_>>();

        log::info!("client ({handle}) connecting ... (ips: {addrs:?})");
        let (conn, addr) = connect_any(&addrs, ctx.cert.clone(), ADDRESS_TIMEOUT).await?;
        log::info!("client ({handle}) connected @ {addr}");

        ctx.client_manager.set_active_addr(handle, Some(addr));
        // The handshake succeeded. Treat the peer as available until a
        // `Pong(false)` or a timeout says otherwise: waiting for the first
        // Pong would otherwise make `send` fail right after connecting.
        ctx.client_manager.set_alive(handle, true);
        ctx.conns.borrow_mut().insert(addr, conn.clone());
        ctx.fast_conns.borrow_mut().insert(handle, conn.clone());
        ctx.notify_connected(handle, true);

        // The version handshake is deferred until the peer answers a `Ping`
        // with capabilities: releases predating `Hello` disconnect on an
        // unknown event id, which would silently make this connection
        // write-only.
        spawn_local(ping_pong(ctx.clone(), addr, conn.clone()));
        spawn_local(receive_loop(ctx.clone(), handle, addr, conn));
        Ok(())
    }
    .await;

    ctx.connecting.borrow_mut().remove(&handle);
    if result.is_err() && ctx.client_manager.is_active(handle) {
        // Retry in the background even though no connection has been
        // established yet (the peer may still be booting).
        ctx.schedule_reconnect(handle);
    }
    result
}

/// Number of consecutive failed datagram sends tolerated before the
/// connection is closed. A single error is normal on Wi-Fi.
const MAX_PING_SEND_FAILURES: u32 = 4;
/// Missed ping rounds tolerated before closing a connection whose peer has
/// answered at least once.
const MAX_MISSED_ROUNDS: u32 = 2;
/// Missed rounds tolerated before closing a connection whose peer has never
/// answered a ping.
///
/// Some peers accept the DTLS connection but never send anything back on it
/// (observed with upstream Lan Mouse releases when *they* did not initiate
/// the connection): they ignore `Ping` on that socket and reply on their own
/// outgoing one. Closing such a connection after two rounds tears down a
/// perfectly usable one-way link every couple of seconds, so liveness is only
/// enforced after a longer grace period.
const MAX_SILENT_ROUNDS: u32 = 30;

/// Decides when a ping round should tear the connection down.
#[derive(Debug, Default)]
struct PingWatchdog {
    send_failures: u32,
    missed_rounds: u32,
    /// The peer answered at least one ping on this connection.
    ever_answered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PingVerdict {
    Keep,
    Close,
}

impl PingWatchdog {
    fn on_send_error(&mut self) -> PingVerdict {
        self.send_failures += 1;
        if self.send_failures >= MAX_PING_SEND_FAILURES {
            PingVerdict::Close
        } else {
            PingVerdict::Keep
        }
    }

    fn on_send_ok(&mut self) {
        self.send_failures = 0;
    }

    /// Record the outcome of one ping round.
    fn on_round(&mut self, answered: bool) -> PingVerdict {
        if answered {
            self.ever_answered = true;
            self.missed_rounds = 0;
            return PingVerdict::Keep;
        }
        self.missed_rounds += 1;
        let limit = if self.ever_answered {
            MAX_MISSED_ROUNDS
        } else {
            MAX_SILENT_ROUNDS
        };
        if self.missed_rounds >= limit {
            PingVerdict::Close
        } else {
            PingVerdict::Keep
        }
    }
}

async fn ping_pong(ctx: ConnCtx, addr: SocketAddr, conn: ArcConn) {
    let mut watchdog = PingWatchdog::default();
    loop {
        let (buf, len) = ProtoEvent::Ping.into();

        // send 4 pings, at least one must be answered
        for _ in 0..4 {
            // Keep the timestamp of the first unanswered ping of the round so
            // the RTT is not reset by the following three probes.
            ctx.ping_sent
                .borrow_mut()
                .entry(addr)
                .or_insert_with(Instant::now);
            if let Err(e) = conn.send(&buf[..len]).await {
                if watchdog.on_send_error() == PingVerdict::Close {
                    log::warn!("{addr}: too many send errors, closing connection");
                    let _ = conn.close().await;
                    return;
                }
                log::warn!("{addr}: send error `{e}` ({})", watchdog.send_failures);
                // Do not tear the connection down on a single error: Wi-Fi
                // links drop individual datagrams all the time.
            } else {
                watchdog.on_send_ok();
                log::trace!("PING >->->->->- {addr}");
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let answered = ctx.ping_response.borrow_mut().remove(&addr);
        if watchdog.on_round(answered) == PingVerdict::Close {
            if watchdog.ever_answered {
                log::warn!("{addr} stopped answering pings, closing connection");
            } else {
                log::warn!("{addr} never answered a ping, closing connection");
            }
            let _ = conn.close().await;
            return;
        }
        if !answered {
            let rounds = watchdog.missed_rounds;
            if watchdog.ever_answered {
                log::warn!("{addr} did not respond to ping round {rounds}");
            } else if rounds == 2 {
                // Log once: some peers never answer on this socket, which is
                // not an error worth repeating every two seconds.
                log::debug!(
                    "{addr} never answers pings on this connection; \
                     keeping it alive for {MAX_SILENT_ROUNDS} rounds"
                );
            }
        }
    }
}

/// Send our build `Hello` once per connection, as soon as the peer proved it
/// can decode events added by this fork.
async fn send_hello_once(ctx: &ConnCtx, addr: SocketAddr, conn: &ArcConn) {
    if !ctx.hello_sent.borrow_mut().insert(addr) {
        return;
    }
    let (buf, len) = ProtoEvent::Hello {
        commit: local_commit(),
        proto_version: lan_mouse_proto::PROTO_VERSION,
        capabilities: lan_mouse_proto::CAPABILITIES,
    }
    .into();
    if let Err(e) = conn.send(&buf[..len]).await {
        ctx.hello_sent.borrow_mut().remove(&addr);
        log::debug!("hello send to {addr} failed: {e}");
    }
}

async fn receive_loop(ctx: ConnCtx, handle: ClientHandle, addr: SocketAddr, conn: ArcConn) {
    let mut buf = [0u8; MAX_CLIPBOARD_FRAME];
    loop {
        let n = match conn.recv(&mut buf).await {
            Ok(n) => n,
            Err(_) => break,
        };
        if n > 0 && is_clipboard_frame(buf[0]) {
            match ClipboardFrame::decode(&buf[..n]) {
                Ok(frame) => ctx
                    .clipboard_tx
                    .send((handle, frame))
                    .ok()
                    .unwrap_or_default(),
                Err(e) => log::debug!("ignoring undecodable clipboard frame from {addr}: {e}"),
            }
            continue;
        }
        match ProtoEvent::try_from(&buf[..n]) {
            Ok(event) => {
                log::trace!("{addr} <==<==<== {event}");
                match event {
                    ProtoEvent::Pong {
                        alive,
                        capabilities,
                    } => {
                        ctx.client_manager.set_active_addr(handle, Some(addr));
                        ctx.client_manager.set_alive(handle, alive);
                        ctx.ping_response.borrow_mut().insert(addr);
                        if let Some(sent) = ctx.ping_sent.borrow_mut().remove(&addr) {
                            let rtt = sent.elapsed().as_millis().min(u32::MAX as u128) as u32;
                            ctx.client_manager.set_rtt_ms(handle, Some(rtt));
                        }
                        if let Some(capabilities) = capabilities {
                            ctx.client_manager
                                .set_peer_capabilities(handle, capabilities);
                            send_hello_once(&ctx, addr, &conn).await;
                        }
                    }
                    ProtoEvent::Hello {
                        commit,
                        proto_version,
                        capabilities,
                    } => {
                        ctx.client_manager.set_peer_hello(
                            handle,
                            Some(commit),
                            proto_version,
                            capabilities,
                        );
                    }
                    event => ctx.tx.send((handle, event)).ok().unwrap_or_default(),
                }
            }
            // Skip undecodable datagrams without dropping the
            // connection. Each DTLS recv is one framed message, so
            // skipping is safe and keeps us forward-compatible with
            // peers that send event types we don't yet know about.
            Err(e) => log::debug!("ignoring undecodable event from {addr}: {e}"),
        }
    }
    log::warn!("recv error");
    ctx.disconnect(handle, addr);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_closes_only_after_a_peer_that_answered_goes_quiet() {
        let mut watchdog = PingWatchdog::default();

        // A peer that answers: two missed rounds tear the link down.
        assert_eq!(watchdog.on_round(true), PingVerdict::Keep);
        assert_eq!(watchdog.on_round(false), PingVerdict::Keep);
        assert_eq!(watchdog.on_round(false), PingVerdict::Close);
    }

    #[test]
    fn watchdog_tolerates_a_peer_that_never_answers() {
        let mut watchdog = PingWatchdog::default();
        for round in 1..MAX_SILENT_ROUNDS {
            assert_eq!(
                watchdog.on_round(false),
                PingVerdict::Keep,
                "round {round} must not close a one-way connection"
            );
        }
        assert_eq!(watchdog.on_round(false), PingVerdict::Close);
    }

    #[test]
    fn watchdog_restarts_after_an_answer() {
        let mut watchdog = PingWatchdog::default();
        for _ in 0..(MAX_SILENT_ROUNDS - 1) {
            watchdog.on_round(false);
        }
        // The peer wakes up: the silent grace resets.
        assert_eq!(watchdog.on_round(true), PingVerdict::Keep);
        assert!(watchdog.ever_answered);
        assert_eq!(watchdog.missed_rounds, 0);
        // From now on the fast two-round limit applies.
        assert_eq!(watchdog.on_round(false), PingVerdict::Keep);
        assert_eq!(watchdog.on_round(false), PingVerdict::Close);
    }

    #[test]
    fn watchdog_closes_after_repeated_send_errors() {
        let mut watchdog = PingWatchdog::default();
        for _ in 1..MAX_PING_SEND_FAILURES {
            assert_eq!(watchdog.on_send_error(), PingVerdict::Keep);
        }
        assert_eq!(watchdog.on_send_error(), PingVerdict::Close);

        // A successful send clears the counter.
        let mut watchdog = PingWatchdog::default();
        watchdog.on_send_error();
        watchdog.on_send_error();
        watchdog.on_send_ok();
        assert_eq!(watchdog.send_failures, 0);
    }
    use crate::client::ClientManager;
    use crate::config::ConfigClient;
    use crate::crypto;
    use crate::emulation::{Emulation, EmulationEvent};
    use crate::listen::{LanMouseListener, ListenEvent};
    use futures::StreamExt;
    use lan_mouse_ipc::Position as IpcPosition;
    use lan_mouse_proto::Position as ProtoPosition;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::{Arc, RwLock};
    use tokio::task::LocalSet;
    use webrtc_dtls::config::{Config as DtlsConfig, ExtendedMasterSecretType};
    use webrtc_dtls::conn::DTLSConn;
    use webrtc_util::Conn;

    fn unique_cert(name: &str) -> Certificate {
        let dir = std::env::temp_dir().join("lan-mouse-tests");
        std::fs::create_dir_all(&dir).expect("test dir");
        let path = dir.join(format!("cert-{}-{name}.pem", std::process::id()));
        let _ = std::fs::remove_file(&path);
        crypto::generate_key_and_cert(&path).expect("generate certificate")
    }

    static PORT_COUNTER: AtomicU16 = AtomicU16::new(0);

    fn free_udp_port() -> u16 {
        loop {
            let offset = PORT_COUNTER.fetch_add(1, Ordering::Relaxed);
            let port = 30000 + (offset % 20000);
            if let Ok(socket) = std::net::UdpSocket::bind(("127.0.0.1", port)) {
                drop(socket);
                return port;
            }
        }
    }

    /// Dial a listener directly with a raw DTLS client, bypassing
    /// [`LanMouseConnection`], so tests can send hand-crafted datagrams.
    async fn raw_dtls_client(port: u16, cert: Certificate) -> Arc<DTLSConn> {
        let udp = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        udp.connect(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
            .await
            .expect("connect");
        let config = DtlsConfig {
            certificates: vec![cert],
            server_name: "ignored".to_owned(),
            insecure_skip_verify: true,
            extended_master_secret: ExtendedMasterSecretType::Require,
            ..Default::default()
        };
        Arc::new(
            DTLSConn::new(udp, config, true, None)
                .await
                .expect("dtls handshake"),
        )
    }

    async fn raw_send(conn: &DTLSConn, event: ProtoEvent) {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        conn.send(&buf[..len]).await.expect("send");
    }

    async fn raw_recv(conn: &DTLSConn) -> ProtoEvent {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), conn.recv(&mut buf))
            .await
            .expect("timed out waiting for a reply")
            .expect("recv");
        ProtoEvent::try_from(&buf[..n]).expect("decode reply")
    }

    fn client_manager_for(port: u16) -> (ClientManager, ClientHandle) {
        let manager = ClientManager::default();
        let handle = manager.add_with_config(ConfigClient {
            ips: HashSet::from([IpAddr::V4(Ipv4Addr::LOCALHOST)]),
            hostname: None,
            port,
            pos: IpcPosition::Right,
            active: true,
            enter_hook: None,
            leave_hook: None,
            sensitivity: 100,
        });
        (manager, handle)
    }

    async fn listener_with_authorized(
        port: u16,
        cert: Certificate,
        authorized: Vec<String>,
    ) -> LanMouseListener {
        let keys = authorized
            .into_iter()
            .enumerate()
            .map(|(i, fp)| (fp, format!("peer-{i}")))
            .collect::<HashMap<_, _>>();
        LanMouseListener::new(
            port,
            cert,
            Arc::new(RwLock::new(keys)),
            Arc::new(RwLock::new(Vec::new())),
            Default::default(),
        )
        .await
        .expect("listener")
    }

    async fn expect_event(listener: &mut LanMouseListener, matches: impl Fn(&ProtoEvent) -> bool) {
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(10), listener.next())
                .await
                .expect("timed out waiting for event")
                .expect("listener stream closed");
            match event {
                ListenEvent::Accept { .. } => continue,
                ListenEvent::AutoAuthorized { .. } => continue,
                ListenEvent::Clipboard { .. } => continue,
                ListenEvent::Rejected { fingerprint, .. } => {
                    panic!("peer rejected: {fingerprint}")
                }
                ListenEvent::Msg { event, .. } => {
                    if matches(&event) {
                        return;
                    }
                }
            }
        }
    }

    /// Both directions must work with the same binary: each instance both
    /// listens and dials out. This is the regression test for the "it is a
    /// server and a client" misunderstanding and for the wire protocol.
    #[tokio::test(flavor = "current_thread")]
    async fn loopback_is_bidirectional() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port_a = free_udp_port();
                let port_b = free_udp_port();

                let cert_a = unique_cert("a");
                let cert_b = unique_cert("b");
                let fp_a = crypto::certificate_fingerprint(&cert_a);
                let fp_b = crypto::certificate_fingerprint(&cert_b);

                let mut listener_a =
                    listener_with_authorized(port_a, cert_a.clone(), vec![fp_b.clone()]).await;
                let mut listener_b =
                    listener_with_authorized(port_b, cert_b.clone(), vec![fp_a.clone()]).await;

                // A -> B
                let (manager_a, handle_a) = client_manager_for(port_b);
                let conn_a = LanMouseConnection::new(cert_a.clone(), manager_a, 2000);
                conn_a
                    .send(
                        ProtoEvent::Enter {
                            pos: ProtoPosition::Right,
                            serial: 1,
                        },
                        handle_a,
                    )
                    .await
                    .expect("A -> B enter");
                conn_a
                    .send(
                        ProtoEvent::Input(input_event::Event::Pointer(
                            input_event::PointerEvent::Motion {
                                time: 1,
                                dx: 5.0,
                                dy: -3.0,
                            },
                        )),
                        handle_a,
                    )
                    .await
                    .expect("A -> B motion");
                expect_event(&mut listener_b, |e| {
                    matches!(e, ProtoEvent::Enter { serial: 1, .. })
                })
                .await;
                expect_event(&mut listener_b, |e| {
                    matches!(
                        e,
                        ProtoEvent::Input(input_event::Event::Pointer(
                            input_event::PointerEvent::Motion { .. }
                        ))
                    )
                })
                .await;

                // B -> A (the reverse direction, using its own outgoing connection)
                let (manager_b, handle_b) = client_manager_for(port_a);
                let conn_b = LanMouseConnection::new(cert_b.clone(), manager_b, 2000);
                conn_b
                    .send(
                        ProtoEvent::Enter {
                            pos: ProtoPosition::Left,
                            serial: 2,
                        },
                        handle_b,
                    )
                    .await
                    .expect("B -> A enter");
                expect_event(&mut listener_a, |e| {
                    matches!(e, ProtoEvent::Enter { serial: 2, .. })
                })
                .await;

                listener_a.terminate().await;
                listener_b.terminate().await;
            })
            .await;
    }
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "current_thread")]
    async fn ipv6_loopback_works() {
        // Skip when the host has no IPv6 loopback.
        if std::net::UdpSocket::bind("[::1]:0").is_err() {
            return;
        }
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("ipv6-listener");
                let client_cert = unique_cert("ipv6-client");
                let fp = crypto::certificate_fingerprint(&client_cert);
                let mut listener = listener_with_authorized(port, listener_cert, vec![fp]).await;

                let manager = ClientManager::default();
                let handle = manager.add_with_config(ConfigClient {
                    ips: HashSet::from([IpAddr::V6(Ipv6Addr::LOCALHOST)]),
                    hostname: None,
                    port,
                    pos: IpcPosition::Right,
                    active: true,
                    enter_hook: None,
                    leave_hook: None,
                    sensitivity: 100,
                });
                let conn = LanMouseConnection::new(client_cert, manager, 2000);
                conn.send(
                    ProtoEvent::Enter {
                        pos: ProtoPosition::Right,
                        serial: 99,
                    },
                    handle,
                )
                .await
                .expect("IPv6 connect");
                expect_event(&mut listener, |event| {
                    matches!(event, ProtoEvent::Enter { serial: 99, .. })
                })
                .await;
                listener.terminate().await;
            })
            .await;
    }

    /// A peer may accept the DTLS connection and read it without ever
    /// replying (upstream releases do exactly that when they did not
    /// initiate the connection). Two silent ping rounds used to close such a
    /// link every few seconds, which froze the pointer at the screen edge.
    #[tokio::test(flavor = "current_thread")]
    async fn silent_listener_keeps_the_link_alive() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("silent-listener");
                let client_cert = unique_cert("silent-client");
                let fp = crypto::certificate_fingerprint(&client_cert);
                let mut listener = listener_with_authorized(port, listener_cert, vec![fp]).await;

                let (manager, handle) = client_manager_for(port);
                let conn = LanMouseConnection::new(client_cert, manager.clone(), 2000);

                conn.send(
                    ProtoEvent::Enter {
                        pos: ProtoPosition::Right,
                        serial: 7,
                    },
                    handle,
                )
                .await
                .expect("enter");
                expect_event(&mut listener, |event| {
                    matches!(event, ProtoEvent::Enter { serial: 7, .. })
                })
                .await;

                // No emulation task runs on the listener side, so nothing
                // ever answers a `Ping`. Stay quiet for longer than the old
                // two-round limit (about four seconds), draining whatever
                // arrives to check for a premature `Hello`.
                let mut seen_hello = false;
                let quiet_until = Instant::now() + Duration::from_secs(5);
                while Instant::now() < quiet_until {
                    match tokio::time::timeout(Duration::from_millis(100), listener.next()).await {
                        Ok(Some(ListenEvent::Msg { event, .. })) => {
                            if matches!(event, ProtoEvent::Hello { .. }) {
                                seen_hello = true;
                            }
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                        Err(_) => {}
                    }
                }
                assert!(
                    manager.active_addr(handle).is_some(),
                    "the one-way link must survive unanswered pings"
                );

                // The version `Hello` must not have been sent: an event id
                // this fork added makes older releases drop the connection.
                assert!(
                    !seen_hello,
                    "a Hello must never be sent to a peer that did not                      advertise capabilities"
                );

                // Input still flows: the peer reads the connection even
                // though it never writes to it.
                conn.send(
                    ProtoEvent::Input(input_event::Event::Pointer(
                        input_event::PointerEvent::Motion {
                            time: 2,
                            dx: 4.0,
                            dy: 1.0,
                        },
                    )),
                    handle,
                )
                .await
                .expect("motion after the silent rounds");
                expect_event(&mut listener, |event| matches!(event, ProtoEvent::Input(_))).await;

                listener.terminate().await;
            })
            .await;
    }

    /// A peer from this fork advertises its capabilities in the `Pong` for
    /// our first `Ping`; only then do we send the `Hello` carrying our build
    /// hash, because older releases disconnect on the unknown event id.
    #[tokio::test(flavor = "current_thread")]
    async fn capabilities_are_learned_from_pong_before_the_hello() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("caps-listener");
                let client_cert = unique_cert("caps-client");
                let fp = crypto::certificate_fingerprint(&client_cert);
                let listener = listener_with_authorized(port, listener_cert, vec![fp]).await;

                let (clipboard_tx, _clipboard_rx) = local_channel::mpsc::channel();
                let mut emulation = Emulation::new(
                    Some(input_emulation::Backend::Dummy),
                    listener,
                    clipboard_tx,
                );

                let (manager, handle) = client_manager_for(port);
                let conn = LanMouseConnection::new(client_cert, manager.clone(), 2000);
                conn.send(
                    ProtoEvent::Enter {
                        pos: ProtoPosition::Right,
                        serial: 3,
                    },
                    handle,
                )
                .await
                .expect("enter");

                let deadline = Instant::now() + Duration::from_secs(10);
                let mut hello_caps = None;
                while Instant::now() < deadline {
                    match tokio::time::timeout(Duration::from_millis(500), emulation.event()).await
                    {
                        Ok(EmulationEvent::PeerHello { capabilities, .. }) => {
                            hello_caps = Some(capabilities);
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => {}
                    }
                }

                assert_eq!(
                    hello_caps,
                    Some(lan_mouse_proto::CAPABILITIES),
                    "the Hello must carry our capabilities once the peer proved it speaks them"
                );
                assert_eq!(
                    manager.peer_capabilities(handle),
                    Some(lan_mouse_proto::CAPABILITIES),
                    "capabilities learned from the Pong are stored before the Hello"
                );
                assert!(manager.peer_supports(handle, lan_mouse_proto::capabilities::ENTER_ACK));

                emulation.terminate().await;
            })
            .await;
    }

    /// An address in the allowlist is trusted on first use: the handshake
    /// succeeds without a prompt and the certificate is stored.
    #[tokio::test(flavor = "current_thread")]
    async fn allowlisted_address_is_authorized_without_a_prompt() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("trusted-listener");
                let client_cert = unique_cert("trusted-client");
                let fingerprint = crypto::certificate_fingerprint(&client_cert);

                let authorized: Arc<RwLock<HashMap<String, String>>> =
                    Arc::new(RwLock::new(HashMap::new()));
                let trusted = Arc::new(RwLock::new(vec![
                    crate::trusted::TrustedNet::parse("127.0.0.1").expect("cidr"),
                ]));
                let mut listener = LanMouseListener::new(
                    port,
                    listener_cert,
                    authorized.clone(),
                    trusted,
                    Default::default(),
                )
                .await
                .expect("listener");

                let (manager, handle) = client_manager_for(port);
                let conn = LanMouseConnection::new(client_cert, manager, 2000);
                conn.send(
                    ProtoEvent::Enter {
                        pos: ProtoPosition::Right,
                        serial: 5,
                    },
                    handle,
                )
                .await
                .expect("allowlisted peer connects");

                // The listener reports the automatic decision and then the
                // usual acceptance.
                let mut auto = false;
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline && !auto {
                    let event = tokio::time::timeout(Duration::from_secs(10), listener.next())
                        .await
                        .expect("timed out")
                        .expect("listener closed");
                    match event {
                        ListenEvent::AutoAuthorized {
                            fingerprint: reported,
                            ..
                        } => {
                            assert_eq!(reported, fingerprint);
                            auto = true;
                        }
                        ListenEvent::Rejected { .. } => {
                            panic!("an allowlisted address must not be rejected")
                        }
                        _ => {}
                    }
                }
                assert!(auto, "expected an automatic authorization");
                assert!(
                    authorized.read().expect("lock").contains_key(&fingerprint),
                    "the certificate must be stored for later connections"
                );

                listener.terminate().await;
            })
            .await;
    }

    /// While pairing mode is open, any certificate is accepted and stored, so
    /// the other machine needs no mouse to approve the request.
    #[tokio::test(flavor = "current_thread")]
    async fn pairing_mode_accepts_any_device() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("pairing-listener");
                let client_cert = unique_cert("pairing-client");
                let fingerprint = crypto::certificate_fingerprint(&client_cert);

                let authorized: Arc<RwLock<HashMap<String, String>>> =
                    Arc::new(RwLock::new(HashMap::new()));
                let pairing: Arc<RwLock<Option<Instant>>> =
                    Arc::new(RwLock::new(Some(Instant::now() + Duration::from_secs(60))));
                let mut listener = LanMouseListener::new(
                    port,
                    listener_cert,
                    authorized.clone(),
                    Arc::new(RwLock::new(Vec::new())),
                    pairing,
                )
                .await
                .expect("listener");

                let (manager, handle) = client_manager_for(port);
                let conn = LanMouseConnection::new(client_cert, manager, 2000);
                conn.send(
                    ProtoEvent::Enter {
                        pos: ProtoPosition::Right,
                        serial: 9,
                    },
                    handle,
                )
                .await
                .expect("pairing accepts the peer");

                let mut accepted = false;
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline && !accepted {
                    let event = tokio::time::timeout(Duration::from_secs(10), listener.next())
                        .await
                        .expect("timed out")
                        .expect("listener closed");
                    match event {
                        ListenEvent::AutoAuthorized {
                            fingerprint: reported,
                            ..
                        } => {
                            assert_eq!(reported, fingerprint);
                            accepted = true;
                        }
                        ListenEvent::Rejected { .. } => panic!("pairing must not reject"),
                        _ => {}
                    }
                }
                assert!(
                    accepted,
                    "expected the pairing window to authorize the peer"
                );
                assert!(authorized.read().expect("lock").contains_key(&fingerprint));
                listener.terminate().await;
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unauthorized_peer_is_rejected() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("reject-listener");
                let client_cert = unique_cert("reject-client");
                let client_fp = crypto::certificate_fingerprint(&client_cert);

                // No authorized fingerprints on the listener.
                let mut listener = listener_with_authorized(port, listener_cert, Vec::new()).await;

                let (manager, handle) = client_manager_for(port);
                let conn = LanMouseConnection::new(client_cert, manager, 500);
                let result = conn.send(ProtoEvent::Ping, handle).await;
                assert!(result.is_err(), "the peer must not be reachable");

                let mut rejected = false;
                for _ in 0..5 {
                    let event =
                        tokio::time::timeout(std::time::Duration::from_secs(5), listener.next())
                            .await
                            .expect("timeout")
                            .expect("listener closed");
                    match event {
                        ListenEvent::AutoAuthorized { .. } => {}
                        ListenEvent::Rejected { fingerprint, .. } => {
                            assert_eq!(fingerprint, client_fp);
                            rejected = true;
                            break;
                        }
                        ListenEvent::Accept { .. } => panic!("unauthorized peer was accepted"),
                        ListenEvent::Msg { .. } | ListenEvent::Clipboard { .. } => {}
                    }
                }
                assert!(rejected, "listener never reported the rejection");
                listener.terminate().await;
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn send_now_never_waits_for_a_connection() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let (manager, handle) = client_manager_for(port);
                let conn = LanMouseConnection::new(unique_cert("send-now"), manager, 5_000);

                let start = Instant::now();
                let result = conn.send_now(ProtoEvent::Ping, handle).await;
                assert!(result.is_err());
                assert!(
                    start.elapsed() < Duration::from_millis(250),
                    "send_now blocked for {:?}",
                    start.elapsed()
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn one_listener_serves_two_outgoing_connections() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("multi-listener");
                let cert_a = unique_cert("multi-a");
                let cert_b = unique_cert("multi-b");
                let fp_a = crypto::certificate_fingerprint(&cert_a);
                let fp_b = crypto::certificate_fingerprint(&cert_b);

                let mut listener =
                    listener_with_authorized(port, listener_cert, vec![fp_a, fp_b]).await;

                let (manager_a, handle_a) = client_manager_for(port);
                let conn_a = LanMouseConnection::new(cert_a, manager_a, 2000);
                conn_a
                    .send(
                        ProtoEvent::Enter {
                            pos: ProtoPosition::Left,
                            serial: 11,
                        },
                        handle_a,
                    )
                    .await
                    .expect("client A");

                let (manager_b, handle_b) = client_manager_for(port);
                let conn_b = LanMouseConnection::new(cert_b, manager_b, 2000);
                conn_b
                    .send(
                        ProtoEvent::Enter {
                            pos: ProtoPosition::Right,
                            serial: 22,
                        },
                        handle_b,
                    )
                    .await
                    .expect("client B");

                let mut serials = Vec::new();
                while serials.len() < 2 {
                    let event =
                        tokio::time::timeout(std::time::Duration::from_secs(10), listener.next())
                            .await
                            .expect("timeout")
                            .expect("listener closed");
                    if let ListenEvent::Msg {
                        event: ProtoEvent::Enter { serial, .. },
                        ..
                    } = event
                    {
                        serials.push(serial);
                    }
                }
                serials.sort();
                assert_eq!(serials, vec![11, 22]);
                listener.terminate().await;
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn legacy_hello_is_accepted_end_to_end() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("legacy-listener");
                let client_cert = unique_cert("legacy-client");
                let fp = crypto::certificate_fingerprint(&client_cert);
                let listener = listener_with_authorized(port, listener_cert, vec![fp]).await;

                let (clipboard_tx, _clipboard_rx) = local_channel::mpsc::channel();
                let mut emulation = Emulation::new(
                    Some(input_emulation::Backend::Dummy),
                    listener,
                    clipboard_tx,
                );
                let client = raw_dtls_client(port, client_cert).await;

                // The old wire format: type byte + 8 byte commit, no version
                // and no capabilities.
                let mut legacy = vec![11u8];
                legacy.extend_from_slice(b"abcdefgh");
                client.send(&legacy).await.expect("send legacy hello");

                // The reply is always the extended format.
                match raw_recv(client.as_ref()).await {
                    ProtoEvent::Hello {
                        proto_version,
                        capabilities,
                        ..
                    } => {
                        assert_eq!(proto_version, lan_mouse_proto::PROTO_VERSION);
                        assert_eq!(capabilities, lan_mouse_proto::CAPABILITIES);
                    }
                    other => panic!("expected Hello reply, got {other}"),
                }

                // And the service sees the legacy metadata as version 0.
                loop {
                    let event =
                        tokio::time::timeout(std::time::Duration::from_secs(5), emulation.event())
                            .await
                            .expect("timed out waiting for the peer hello");
                    match event {
                        EmulationEvent::PeerHello {
                            commit,
                            proto_version,
                            capabilities,
                            ..
                        } => {
                            assert_eq!(&commit, b"abcdefgh");
                            assert_eq!(proto_version, 0);
                            assert_eq!(capabilities, 0);
                            break;
                        }
                        EmulationEvent::Entered { .. } => {
                            panic!("a Hello must not trigger a handoff")
                        }
                        _ => {}
                    }
                }

                let _ = client.close().await;
                emulation.terminate().await;
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emulation_acks_enter_and_deduplicates_retransmissions() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("emulation-listener");
                let client_cert = unique_cert("emulation-client");
                let fp = crypto::certificate_fingerprint(&client_cert);
                let listener = listener_with_authorized(port, listener_cert, vec![fp]).await;

                let (clipboard_tx, _clipboard_rx) = local_channel::mpsc::channel();
                let mut emulation = Emulation::new(
                    Some(input_emulation::Backend::Dummy),
                    listener,
                    clipboard_tx,
                );

                let client = raw_dtls_client(port, client_cert).await;

                raw_send(
                    client.as_ref(),
                    ProtoEvent::Enter {
                        pos: ProtoPosition::Top,
                        serial: 42,
                    },
                )
                .await;
                assert!(matches!(
                    raw_recv(client.as_ref()).await,
                    ProtoEvent::Ack(42)
                ));
                expect_entered(&mut emulation).await;

                // Retransmission with the same serial: acknowledged again,
                // but it must not trigger another handoff.
                raw_send(
                    client.as_ref(),
                    ProtoEvent::Enter {
                        pos: ProtoPosition::Top,
                        serial: 42,
                    },
                )
                .await;
                assert!(matches!(
                    raw_recv(client.as_ref()).await,
                    ProtoEvent::Ack(42)
                ));

                // A new serial must hand off again.
                raw_send(
                    client.as_ref(),
                    ProtoEvent::Enter {
                        pos: ProtoPosition::Top,
                        serial: 43,
                    },
                )
                .await;
                assert!(matches!(
                    raw_recv(client.as_ref()).await,
                    ProtoEvent::Ack(43)
                ));

                let mut releases = 0;
                loop {
                    let event =
                        tokio::time::timeout(std::time::Duration::from_secs(5), emulation.event())
                            .await
                            .expect("timed out waiting for Entered");
                    match event {
                        EmulationEvent::ReleaseNotify => releases += 1,
                        EmulationEvent::Entered { .. } => break,
                        _ => {}
                    }
                }
                assert_eq!(releases, 1, "only the new serial may trigger a handoff");

                let _ = client.close().await;
                emulation.terminate().await;
            })
            .await;
    }

    async fn expect_entered(emulation: &mut Emulation) {
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), emulation.event())
                .await
                .expect("timed out waiting for Entered");
            if let EmulationEvent::Entered { .. } = event {
                return;
            }
        }
    }
}
