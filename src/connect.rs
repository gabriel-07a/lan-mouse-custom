use crate::client::ClientManager;
use crate::config::local_commit;
use crate::crypto;
use crate::devices::DeviceStore;
use lan_mouse_ipc::{ClientHandle, DEFAULT_PORT};
use lan_mouse_proto::{
    MAX_EVENT_SIZE, ProtoEvent,
    clipboard::{ClipboardFrame, MAX_CLIPBOARD_FRAME, is_clipboard_frame},
};
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::CertificateDer;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    rc::Rc,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{net::UdpSocket, sync::watch, task::spawn_local};
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
    #[error("peer certificate mismatch: expected {expected}, got {actual}")]
    CertificateMismatch { expected: String, actual: String },
}

/// Per-address DTLS handshake budget. Addresses are dialed one after the
/// other (see [`dial_order`]), so a dead preferred address delays the switch to
/// the failover address by at most this budget.
const ADDRESS_TIMEOUT: Duration = Duration::from_secs(2);

/// Backoff for the background reconnect loop while a client stays active.
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_millis(250);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);

type ArcConn = Arc<dyn Conn + Send + Sync>;

/// One established transport for a configured client.
///
/// A client (one logical peer, identified by its config entry and, once
/// learned, its certificate fingerprint) owns **at most one** of these. The
/// generation makes every task that belongs to a transport able to tell
/// whether it is still the current one: a stale ping/receive loop must never
/// write connection state for a transport that was already replaced.
#[derive(Clone)]
struct ActiveConnection {
    /// Monotonic id of this transport; changes on every (re)connect.
    generation: u64,
    /// The peer address this transport was established to.
    addr: SocketAddr,
    conn: ArcConn,
}

/// Shared state of the outgoing connection layer.
///
/// Everything in this module runs on the single-threaded [`tokio::task::LocalSet`],
/// so interior mutability via `Rc<RefCell<_>>` is enough and keeps the hot
/// send path free of async locks.
#[derive(Clone)]
struct ConnCtx {
    cert: Certificate,
    client_manager: ClientManager,
    /// the single live transport per client (source of truth)
    active: Rc<RefCell<HashMap<ClientHandle, ActiveConnection>>>,
    /// source of monotonic [`ActiveConnection::generation`] ids
    generation: Rc<Cell<u64>>,
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
    /// notifies the service when a client's connection/liveness/RTT state changes
    state_tx: Sender<ClientHandle>,
    /// addresses that answered the current ping round
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    /// timestamp of the last ping sent, used to derive RTT from `Pong`
    ping_sent: Rc<RefCell<HashMap<SocketAddr, Instant>>>,
    /// addresses that already received our build `Hello`. A `Hello` is only
    /// sent once the peer proved it understands the extension by answering a
    /// `Ping` with capabilities: older releases close the connection when
    /// they receive an event id they do not know.
    hello_sent: Rc<RefCell<HashSet<SocketAddr>>>,
    /// device policies used to compute the permission grants announced to
    /// peers; `None` disables the permission negotiation (tests, legacy paths)
    devices: Option<Arc<RwLock<DeviceStore>>>,
    /// permission session id per peer address, stable for a connection
    auth_sessions: Rc<RefCell<HashMap<SocketAddr, u128>>>,
    /// Permission bits last announced to a peer address, with the time of the
    /// announcement. AuthRequest is idempotent and the ping round runs every
    /// ~2 s; repeating it on every Pong produced identical AuthRequest/
    /// AuthResponse/PeerPermissions traffic (and an audit-log record) forever.
    auth_announced: Rc<RefCell<HashMap<SocketAddr, AnnouncedPermissions>>>,
    /// how long a single `send` blocks waiting for a connection
    grace: Duration,
}

/// Permission bits last announced to one peer, and when.
#[derive(Clone, Copy, Debug)]
struct AnnouncedPermissions {
    incoming: u8,
    outgoing: u8,
    sent_at: Instant,
}

impl ConnCtx {
    fn notify_connected(&self, handle: ClientHandle, connected: bool) {
        if let Some(tx) = self.waiters.borrow().get(&handle) {
            tx.send_replace(connected);
        }
        let _ = self.state_tx.send(handle);
    }

    fn notify_state_changed(&self, handle: ClientHandle) {
        let _ = self.state_tx.send(handle);
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
        // A client that is not active must never be dialed. The capture task
        // retries `ensure_connected` on every failed send; before this gate a
        // peer's explicit disconnect (AuthRevoke -> `deactivate_client`) was
        // undone by that retry and the controller silently re-established the
        // session behind the UI's back. Only an explicit activation may start
        // a new session.
        if !self.client_manager.is_active(handle) {
            log::debug!("client {handle}: refusing to connect, client is not active");
            return;
        }
        if self.active(handle).is_some() {
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
                if ctx.active(handle).is_some() {
                    break;
                }
                ctx.start_connecting(handle);
                delay = (delay * 2).min(RECONNECT_BACKOFF_MAX);
            }
            ctx.reconnecting.borrow_mut().remove(&handle);
        });
    }

    /// The live transport of `handle`, if any.
    fn active(&self, handle: ClientHandle) -> Option<ActiveConnection> {
        self.active.borrow().get(&handle).cloned()
    }

    /// Whether `generation` is still the live transport of `handle`.
    fn is_current(&self, handle: ClientHandle, generation: u64) -> bool {
        self.active
            .borrow()
            .get(&handle)
            .is_some_and(|active| active.generation == generation)
    }

    fn next_generation(&self) -> u64 {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        generation
    }

    /// Install a new transport, returning the one it replaced (if any).
    fn install(&self, handle: ClientHandle, active: ActiveConnection) -> Option<ActiveConnection> {
        self.active.borrow_mut().insert(handle, active)
    }

    /// Remove the live transport only if it is still `generation`.
    fn take_if_current(&self, handle: ClientHandle, generation: u64) -> Option<ActiveConnection> {
        let mut active = self.active.borrow_mut();
        match active.get(&handle) {
            Some(current) if current.generation == generation => active.remove(&handle),
            _ => None,
        }
    }

    /// Tear down the transport of `handle` if it is still `generation`.
    ///
    /// The state is cleared before the transport is closed so a replacement
    /// connection can be installed while the close is still in flight. A
    /// stale generation is a no-op: that is what keeps an old ping/receive
    /// loop from fighting with the current transport over `active_addr`.
    async fn disconnect(&self, handle: ClientHandle, generation: u64) {
        let Some(active) = self.take_if_current(handle, generation) else {
            return;
        };
        log::warn!("client ({handle}) @ {} connection closed", active.addr);
        self.client_manager.set_active_addr(handle, None);
        self.client_manager.set_alive(handle, false);
        self.client_manager.set_peer_hello(handle, None, 0, 0);
        self.client_manager.set_rtt_ms(handle, None);
        // No transport means no round-trip proof: the frontend must not keep
        // showing "Connected" based on a Pong from the closed link.
        self.client_manager.set_last_pong_at(handle, None);
        self.ping_sent.borrow_mut().remove(&active.addr);
        self.ping_response.borrow_mut().remove(&active.addr);
        // A reconnect may legitimately reuse the same ip:port (stateful NAT,
        // fast socket rebinding), so per-address bookkeeping must not outlive
        // the transport it belongs to.
        self.hello_sent.borrow_mut().remove(&active.addr);
        self.auth_sessions.borrow_mut().remove(&active.addr);
        self.auth_announced.borrow_mut().remove(&active.addr);
        self.notify_connected(handle, false);
        let _ = active.conn.close().await;
        self.schedule_reconnect(handle);
    }

    /// Resolve the live connection for `handle`, waiting up to `remaining`
    /// for a connection attempt that is already (or will be) in flight.
    async fn conn_for(
        &self,
        handle: ClientHandle,
        remaining: Duration,
    ) -> Result<ActiveConnection, LanMouseConnectionError> {
        if let Some(active) = self.active(handle) {
            return Ok(active);
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

        self.active(handle)
            .ok_or(LanMouseConnectionError::NotConnected)
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
    state_rx: Option<Receiver<ClientHandle>>,
}

impl LanMouseConnection {
    pub(crate) fn new(
        cert: Certificate,
        client_manager: ClientManager,
        connect_grace_ms: u64,
    ) -> Self {
        let (recv_tx, recv_rx) = channel();
        let (clipboard_tx, clipboard_rx) = channel();
        let (state_tx, state_rx) = channel();
        let ctx = ConnCtx {
            cert,
            client_manager,
            active: Default::default(),
            generation: Default::default(),
            connecting: Default::default(),
            reconnecting: Default::default(),
            waiters: Default::default(),
            tx: recv_tx,
            clipboard_tx,
            state_tx,
            ping_response: Default::default(),
            ping_sent: Default::default(),
            hello_sent: Default::default(),
            devices: None,
            auth_sessions: Default::default(),
            auth_announced: Default::default(),
            grace: Duration::from_millis(connect_grace_ms),
        };
        Self {
            sender: ConnectionSender { ctx },
            recv_rx,
            clipboard_rx: Some(clipboard_rx),
            state_rx: Some(state_rx),
        }
    }

    /// Cloneable send half, used by the clipboard sync task.
    pub(crate) fn sender(&self) -> ConnectionSender {
        self.sender.clone()
    }

    /// Enable permission negotiation: the store is used to compute what this
    /// machine grants the peer, and the result is announced once the peer
    /// advertises the `PERMISSIONS` capability.
    ///
    /// Must be called before cloning the connection (the send half is cloned
    /// by the clipboard/capture tasks right after construction).
    pub(crate) fn set_device_store(&mut self, devices: Arc<RwLock<DeviceStore>>) {
        self.sender.ctx.devices = Some(devices);
    }

    /// Hand the clipboard frame receiver to the clipboard task. The input
    /// capture task keeps the input event receiver.
    pub(crate) fn take_clipboard_rx(&mut self) -> Option<Receiver<(ClientHandle, ClipboardFrame)>> {
        self.clipboard_rx.take()
    }

    /// Hand the connection state receiver to the service loop so it can
    /// broadcast live connection/RTT updates to frontends.
    pub(crate) fn take_state_rx(&mut self) -> Option<Receiver<ClientHandle>> {
        self.state_rx.take()
    }

    /// Explicit user disconnect: tell the peer to stop reconnecting, then
    /// close the live transport. The service deactivates the client first, so
    /// the reconnect loop observes `active == false` and stays stopped.
    pub(crate) async fn disconnect(&self, handle: ClientHandle) {
        let Some(active) = self.sender.ctx.active(handle) else {
            return;
        };
        // `AuthRevoke` exists in both current peers and older forks and is safe
        // to receive (unknown new event ids would close legacy connections).
        // Use the raw send path: the peer may have disabled emulation, which
        // must not prevent the explicit-disconnect notification.
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::AuthRevoke {
            session_id: 0,
            reason: 1,
        }
        .into();
        let _ = self.sender.send_raw(handle, &buf[..len]).await;
        self.sender.ctx.disconnect(handle, active.generation).await;
    }

    /// Close the live outgoing transport of a client without notifying the
    /// peer (used when the client entry is deleted or merged into another one).
    ///
    /// The service removes the client from the manager before calling this, so
    /// the connection layer sees an inactive client and does not schedule a
    /// reconnect.
    pub(crate) async fn drop_transport(&self, handle: ClientHandle) {
        if let Some(active) = self.sender.ctx.active(handle) {
            self.sender.ctx.disconnect(handle, active.generation).await;
        }
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

    /// Whether the peer advertises serialized return hand-off requests
    /// (`Leave(serial)` with a non-zero serial, `Leave(0)` reserved for the
    /// plain release confirmation). Older peers may use `Leave(0)` as the
    /// actual request, so the capture must keep honouring it for them.
    pub(crate) fn peer_supports_serialized_leave(&self, handle: ClientHandle) -> bool {
        self.sender.peer_supports_serialized_leave(handle)
    }

    /// Whether the peer understands `EnterAt` and can continue the pointer
    /// from the exact crossing coordinate instead of the edge centre.
    pub(crate) fn peer_supports_edge_coordinate(&self, handle: ClientHandle) -> bool {
        self.sender.peer_supports_edge_coordinate(handle)
    }

    pub(crate) fn peer_rtt_ms(&self, handle: ClientHandle) -> Option<u32> {
        self.sender.peer_rtt_ms(handle)
    }

    /// Whether a DTLS transport is currently established for `handle`.
    ///
    /// The capture task uses this before arming a grab: with no transport the
    /// `Enter` cannot be delivered, and grabbing the local pointer would leave
    /// it locked at the edge until the retry timeout.
    pub(crate) fn is_connected(&self, handle: ClientHandle) -> bool {
        self.sender.ctx.client_manager.active_addr(handle).is_some()
    }

    #[allow(dead_code)]
    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        self.sender.send(event, handle).await
    }

    pub(crate) async fn send_with_grace(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
        grace: Duration,
    ) -> Result<(), LanMouseConnectionError> {
        self.sender.send_with_grace(event, handle, grace).await
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

    /// Whether the peer advertises serialized return hand-off requests
    /// (`Leave(serial)` with a non-zero serial). Peers without this bit,
    /// including older builds that already acknowledge `Enter`, may still use
    /// `Leave(0)` as the release request.
    pub(crate) fn peer_supports_serialized_leave(&self, handle: ClientHandle) -> bool {
        self.ctx
            .client_manager
            .peer_capabilities(handle)
            .is_some_and(|caps| caps & lan_mouse_proto::capabilities::SERIALIZED_LEAVE != 0)
    }

    /// Whether the peer understands `EnterAt` and can position the pointer at
    /// the exact crossing coordinate.
    pub(crate) fn peer_supports_edge_coordinate(&self, handle: ClientHandle) -> bool {
        self.ctx
            .client_manager
            .peer_capabilities(handle)
            .is_some_and(|caps| caps & lan_mouse_proto::capabilities::EDGE_COORDINATE != 0)
    }

    pub(crate) fn peer_rtt_ms(&self, handle: ClientHandle) -> Option<u32> {
        self.ctx.client_manager.rtt_ms(handle)
    }

    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        self.send_with_grace(event, handle, self.ctx.grace).await
    }

    pub(crate) async fn send_with_grace(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
        grace: Duration,
    ) -> Result<(), LanMouseConnectionError> {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        self.send_bytes(&buf[..len], handle, grace, true).await
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
            let active = self.ctx.conn_for(handle, remaining).await?;

            if require_alive && !self.ctx.client_manager.alive(handle) {
                return Err(LanMouseConnectionError::TargetEmulationDisabled);
            }

            match active.conn.send(buf).await {
                Ok(_) => {
                    log::trace!("{} bytes >->->->->- {}", buf.len(), active.addr);
                    return Ok(());
                }
                Err(e) => {
                    log::warn!("client {handle} failed to send: {e}");
                    self.ctx.disconnect(handle, active.generation).await;
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
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows::Win32::Networking::WinSock::{
            IP_TOS, IPPROTO_IP, IPPROTO_IPV6, IPV6_TCLASS, SOCKET, setsockopt,
        };
        let raw = SOCKET(socket.as_raw_socket() as usize);
        let tos: i32 = 0xB8; // DSCP EF (46) << 2
        let bytes = tos.to_ne_bytes();
        unsafe {
            // IPv4 and IPv6 sockets: the irrelevant call fails harmlessly.
            let _ = setsockopt(raw, IPPROTO_IP.0, IP_TOS, Some(&bytes));
            let _ = setsockopt(raw, IPPROTO_IPV6.0, IPV6_TCLASS, Some(&bytes));
        }
    }
    #[cfg(not(any(unix, windows)))]
    let _ = socket;
}

/// Current wall-clock time in Unix milliseconds.
///
/// `last_pong_at` is a frontend-facing timestamp (not a monotonic `Instant`):
/// the GUI compares it with the system clock, and it must survive being read
/// out of the IPC state snapshot.
fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

/// Fingerprint of the certificate presented by an outgoing connection.
async fn peer_fingerprint(conn: &ArcConn) -> Option<String> {
    let dtls: &DTLSConn = conn.as_any().downcast_ref()?;
    dtls.connection_state()
        .await
        .peer_certificates
        .first()
        .map(|cert| crypto::generate_fingerprint(cert))
}

async fn connect(
    addr: SocketAddr,
    cert: Certificate,
    timeout: Duration,
    expected_fingerprint: Option<String>,
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
    let mut config = Config {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    match expected_fingerprint {
        // Pin the server certificate to the identity we already know for this
        // device. Without this a host that happens to occupy the configured IP
        // (DHCP reuse, ARP/DNS spoofing, wrong machine) would receive our
        // keyboard, mouse and clipboard without any warning.
        Some(expected) => {
            // Self-signed peers cannot pass the standard chain verification,
            // so skip it and pin the certificate with our own callback.
            config.insecure_skip_verify = true;
            config.verify_peer_certificate = Some(Arc::new(
                move |certs: &[Vec<u8>], _chains: &[CertificateDer<'static>]| {
                    let Some(cert) = certs.first() else {
                        return Err(webrtc_dtls::Error::ErrVerifyDataMismatch);
                    };
                    let actual = crypto::generate_fingerprint(cert);
                    if actual == expected {
                        Ok(())
                    } else {
                        log::warn!(
                            "server certificate mismatch: expected {expected}, got {actual}"
                        );
                        Err(webrtc_dtls::Error::ErrVerifyDataMismatch)
                    }
                },
            ));
        }
        // Legacy/TOFU path: no pin known yet. The first successful connection
        // learns the fingerprint (see `connect_to_handle`).
        None => config.insecure_skip_verify = true,
    }
    let timeout = tokio::time::sleep(timeout);
    tokio::select! {
        _ = timeout => Err((addr, LanMouseConnectionError::Timeout)),
        result = DTLSConn::new(conn, config, true, None) => match result {
            Ok(dtls_conn) => Ok((Arc::new(dtls_conn), addr)),
            Err(e) => Err((addr, e.into())),
        }
    }
}

/// Loopback plus the non-loopback IPv4 addresses of this machine.
fn local_ip_addresses() -> std::collections::HashSet<IpAddr> {
    let mut addresses = std::collections::HashSet::from([IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    for interface in crate::discovery::local_interfaces() {
        addresses.insert(IpAddr::V4(interface.addr));
    }
    addresses
}

/// Remove addresses that point back at this machine.
///
/// A saved client can end up with a stale address that was later leased to
/// this machine (DHCP change), or a bad manual entry. Dialing it would connect
/// to our own listener, which shows the permission request on the wrong PC
/// ("it asks me to authorize myself") instead of reaching the peer.
fn dialable_candidates(
    addresses: impl IntoIterator<Item = SocketAddr>,
    own: &std::collections::HashSet<IpAddr>,
) -> Vec<SocketAddr> {
    addresses
        .into_iter()
        .filter(|address| address.ip().is_loopback() || !own.contains(&address.ip()))
        .collect()
}

/// Order the candidate addresses of a client for one connection attempt.
///
/// The address that established the previous session (remembered per client
/// by [`ClientManager::record_working_addr`]) is tried first, then the
/// remaining addresses numerically. The result is deduplicated and
/// deterministic, so a multi-address client never dials the same endpoint
/// twice in one attempt. Callers filter to IPv4 before dialing.
fn dial_order(
    addrs: impl IntoIterator<Item = SocketAddr>,
    preferred: Option<SocketAddr>,
) -> Vec<SocketAddr> {
    let preferred_ip = preferred.map(|addr| addr.ip());
    let mut addrs = addrs.into_iter().collect::<Vec<_>>();
    addrs.sort_by_key(|addr| (u8::from(Some(addr.ip()) != preferred_ip), *addr));
    addrs.dedup();
    addrs
}

/// Build the candidate addresses for the outgoing half of a link.
///
/// The configured/DNS addresses are the base. If the peer has an accepted
/// incoming session, its source address is added too: the peer proved that
/// address can reach us, and hostname resolution may be failing (mDNS/DNS
/// down). That address becomes the preferred candidate when no remembered
/// address is usable. IPv4-only, matching the dial path.
fn outgoing_addrs(
    mut ips: HashSet<IpAddr>,
    port: u16,
    incoming: Option<SocketAddr>,
    preferred: Option<SocketAddr>,
) -> Vec<SocketAddr> {
    if let Some(incoming) = incoming {
        if incoming.ip().is_ipv4() {
            ips.insert(incoming.ip());
        }
    }
    let preferred = preferred
        .filter(|preferred| ips.contains(&preferred.ip()))
        .or_else(|| {
            incoming
                .map(|addr| SocketAddr::new(addr.ip(), port))
                .filter(|candidate| ips.contains(&candidate.ip()))
        });
    dial_order(
        ips.into_iter().map(|ip| SocketAddr::new(ip, port)),
        preferred,
    )
    .into_iter()
    .filter(SocketAddr::is_ipv4)
    .collect()
}

/// Try the candidate addresses one at a time, in [`dial_order`] order, and
/// return the first successful handshake.
///
/// One attempt = at most one session, and the ordering above keeps the chosen
/// address stable. Sequential failover is slower in the worst case (one
/// [`ADDRESS_TIMEOUT`] per dead address), which is the documented trade-off
/// for a stable logical link.
async fn connect_first(
    addrs: impl IntoIterator<Item = SocketAddr>,
    cert: Certificate,
    timeout: Duration,
    expected_fingerprint: Option<String>,
) -> Result<(ArcConn, SocketAddr), LanMouseConnectionError> {
    let mut last_error = LanMouseConnectionError::NotConnected;
    let mut attempted = 0usize;
    for addr in addrs {
        attempted += 1;
        match connect(addr, cert.clone(), timeout, expected_fingerprint.clone()).await {
            Ok(connected) => return Ok(connected),
            Err((addr, e)) => {
                log::warn!("failed to connect to {addr}: `{e}`");
                last_error = e;
            }
        }
    }
    if attempted == 0 {
        return Err(LanMouseConnectionError::NotConnected);
    }
    Err(last_error)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PinDecision {
    Match,
    /// Different certificate, but already a known/authorized device.
    RotateKnown,
    Reject,
}

/// Decide what a presented peer certificate means against the saved pin.
fn pin_decision(expected: &str, actual: &str, actual_known: bool) -> PinDecision {
    if expected.eq_ignore_ascii_case(actual) {
        PinDecision::Match
    } else if actual_known {
        PinDecision::RotateKnown
    } else {
        PinDecision::Reject
    }
}

/// Verify (and, for a known device, refresh) the certificate the peer actually
/// presented.
///
/// The DTLS verifier is the first line of defence, but its callback behaviour
/// differs between the Linux and Windows stacks; enforcing the pin again on the
/// completed connection makes a rotated identity reliable everywhere.
async fn verify_connected_peer(
    ctx: &ConnCtx,
    handle: ClientHandle,
    conn: &ArcConn,
    expected: Option<&str>,
) -> Result<(), LanMouseConnectionError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let Some(actual) = peer_fingerprint(conn).await else {
        let _ = conn.close().await;
        return Err(LanMouseConnectionError::NotConnected);
    };
    let known = ctx
        .devices
        .as_ref()
        .is_some_and(|devices| devices.read().expect("lock").is_known(&actual));
    match pin_decision(expected, &actual, known) {
        PinDecision::Match => Ok(()),
        PinDecision::RotateKnown => {
            log::warn!(
                "client {handle} presented a rotated certificate already known as {actual}; re-pinning and connecting"
            );
            ctx.client_manager
                .set_fingerprint(handle, Some(actual.clone()));
            ctx.notify_state_changed(handle);
            Ok(())
        }
        PinDecision::Reject => {
            log::warn!(
                "client {handle} presented a changed certificate {actual} that is not a known device; rejecting"
            );
            let _ = conn.close().await;
            Err(LanMouseConnectionError::CertificateMismatch {
                expected: expected.to_string(),
                actual,
            })
        }
    }
}

async fn connect_to_handle(
    ctx: &ConnCtx,
    handle: ClientHandle,
) -> Result<(), LanMouseConnectionError> {
    log::info!("client {handle} connecting ...");

    let result = async {
        if ctx.active(handle).is_some() {
            return Ok(());
        }

        let ips = ctx
            .client_manager
            .get_ips(handle)
            .ok_or(LanMouseConnectionError::NotConnected)?;
        let port = ctx.client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let incoming = ctx.client_manager.incoming_addr(handle);
        let mut preferred = ctx.client_manager.preferred_addr(handle);
        if preferred.is_some_and(|preferred| !ips.contains(&preferred.ip())) {
            // The remembered address is no longer part of the client's address
            // set (DNS change, new DHCP lease, edited config): forget it so the
            // failover starts from the documented address preference again.
            ctx.client_manager.clear_preferred_addr(handle);
            preferred = None;
        }
        let addrs = dialable_candidates(
            outgoing_addrs(ips, port, incoming, preferred),
            &local_ip_addresses(),
        );
        if addrs.is_empty() {
            log::warn!(
                "client {handle} has no dialable address: every candidate points at this machine"
            );
            return Err(LanMouseConnectionError::NotConnected);
        }

        log::info!("client ({handle}) connecting ... (ordered addresses: {addrs:?})");
        let expected = ctx.client_manager.fingerprint(handle);
        // Dial without the DTLS verifier callback even when a pin exists: the
        // callback behaves differently across DTLS stacks (Windows can
        // complete or time out a mismatched handshake), and recovering from a
        // rotated certificate needs a second handshake otherwise. The pin is
        // enforced on the completed connection below, before it is installed
        // or any application data is exchanged.
        let (conn, addr) = connect_first(addrs, ctx.cert.clone(), ADDRESS_TIMEOUT, None).await?;
        verify_connected_peer(ctx, handle, &conn, expected.as_deref()).await?;
        log::info!("client ({handle}) connected @ {addr}");

        // A dial can race the peer's AuthRevoke (or a local deactivate). The
        // handshake may already have succeeded while the client was marked
        // inactive; never install a transport for an inactive client.
        if !ctx.client_manager.is_active(handle) {
            log::info!(
                "client {handle}: discarding connection to {addr}, the client was deactivated while dialing"
            );
            let _ = conn.close().await;
            return Ok(());
        }

        // Trust on first use: pin the certificate the peer actually presented
        // so the next connection is verified against it.
        if expected.is_none() {
            if let Some(fingerprint) = peer_fingerprint(&conn).await {
                log::info!(
                    "pinned certificate fingerprint {fingerprint} for client {handle} (TOFU)"
                );
                ctx.client_manager
                    .set_fingerprint(handle, Some(fingerprint));
                ctx.notify_state_changed(handle);
            }
        }

        // Defensive: `connecting` already serializes attempts for one handle,
        // but a duplicate transport must never be installed.
        if let Some(existing) = ctx.active(handle) {
            log::warn!(
                "client {handle}: duplicate connection to {addr} (current: {}), closing it",
                existing.addr
            );
            let _ = conn.close().await;
            return Ok(());
        }

        let generation = ctx.next_generation();
        let active = ActiveConnection {
            generation,
            addr,
            conn,
        };
        if let Some(previous) = ctx.install(handle, active.clone()) {
            // Unreachable with the guard above, but closing the old transport
            // is what makes "one logical peer = one connection" hold even if
            // that guard ever regresses.
            let _ = previous.conn.close().await;
        }
        ctx.client_manager.set_active_addr(handle, Some(addr));
        ctx.client_manager.record_working_addr(handle, addr);
        // The handshake succeeded. Treat the peer as available until a
        // `Pong(false)` or a timeout says otherwise: waiting for the first
        // Pong would otherwise make `send` fail right after connecting.
        ctx.client_manager.set_alive(handle, true);
        ctx.notify_connected(handle, true);

        // The version handshake is deferred until the peer answers a `Ping`
        // with capabilities: releases predating `Hello` disconnect on an
        // unknown event id, which would silently make this connection
        // write-only.
        spawn_local(ping_pong(ctx.clone(), handle, active.clone()));
        spawn_local(receive_loop(ctx.clone(), handle, active));
        Ok(())
    }
    .await;

    ctx.connecting.borrow_mut().remove(&handle);
    match &result {
        Ok(()) => ctx.client_manager.set_last_error(handle, None),
        Err(error) => {
            log::warn!("client {handle} connect attempt failed: {error}");
            ctx.client_manager
                .set_last_error(handle, Some(error.to_string()));
        }
    }
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
    /// Total rounds since this transport was established.
    rounds: u32,
    /// Rounds without a reply since this transport was established.
    lost_rounds: u32,
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
        self.rounds = self.rounds.saturating_add(1);
        if answered {
            self.ever_answered = true;
            self.missed_rounds = 0;
            return PingVerdict::Keep;
        }
        self.lost_rounds = self.lost_rounds.saturating_add(1);
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

    fn loss_percent(&self) -> Option<u8> {
        (self.rounds > 0)
            .then(|| ((u64::from(self.lost_rounds) * 100) / u64::from(self.rounds)) as u8)
    }
}

/// Send keepalive `Ping`s on one transport and close it when the peer stops
/// answering.
///
/// Tied to [`ActiveConnection::generation`]: once a replacement transport is
/// installed, this loop exits quietly instead of touching the client state
/// (which was the source of the IPv4/IPv6 flapping when two transports for the
/// same peer existed).
async fn ping_pong(ctx: ConnCtx, handle: ClientHandle, active: ActiveConnection) {
    let mut watchdog = PingWatchdog::default();
    let addr = active.addr;
    loop {
        if !ctx.is_current(handle, active.generation) {
            return;
        }
        let (buf, len) = ProtoEvent::Ping.into();

        // send 4 pings, at least one must be answered
        for _ in 0..4 {
            if !ctx.is_current(handle, active.generation) {
                return;
            }
            // Keep the timestamp of the first unanswered ping of the round so
            // the RTT is not reset by the following three probes.
            ctx.ping_sent
                .borrow_mut()
                .entry(addr)
                .or_insert_with(Instant::now);
            if let Err(e) = active.conn.send(&buf[..len]).await {
                if watchdog.on_send_error() == PingVerdict::Close {
                    log::warn!("{addr}: too many send errors, closing connection");
                    ctx.disconnect(handle, active.generation).await;
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

        if !ctx.is_current(handle, active.generation) {
            return;
        }
        let answered = ctx.ping_response.borrow_mut().remove(&addr);
        let verdict = watchdog.on_round(answered);
        ctx.client_manager
            .set_loss_percent(handle, watchdog.loss_percent());
        ctx.notify_state_changed(handle);
        if verdict == PingVerdict::Close {
            if watchdog.ever_answered {
                log::warn!("{addr} stopped answering pings, closing connection");
            } else {
                log::warn!("{addr} never answered a ping, closing connection");
            }
            ctx.disconnect(handle, active.generation).await;
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

/// Stable per-connection session id used for permission updates.
fn auth_session(ctx: &ConnCtx, addr: SocketAddr) -> u128 {
    *ctx.auth_sessions
        .borrow_mut()
        .entry(addr)
        .or_insert_with(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default();
            nanos ^ ((addr.port() as u128) << 96) ^ (std::process::id() as u128)
        })
}

/// Announce the local permission grants to the peer.
///
/// Only called after the peer advertised [`capabilities::PERMISSIONS`]:
/// legacy peers close the connection on an unknown event id.
/// How long an unchanged permission announcement is refreshed to recover from
/// a lost datagram (DTLS application data is not retransmitted).
const AUTH_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

async fn send_auth_request(ctx: &ConnCtx, handle: ClientHandle, addr: SocketAddr, conn: &ArcConn) {
    let Some(devices) = &ctx.devices else {
        return;
    };
    let (incoming, outgoing) = match ctx.client_manager.fingerprint(handle) {
        Some(fingerprint) => {
            let store = devices.read().expect("lock");
            (store.incoming(&fingerprint), store.outgoing(&fingerprint))
        }
        None => (
            lan_mouse_ipc::DirectionPolicy::allow_all(),
            lan_mouse_ipc::DirectionPolicy::allow_all(),
        ),
    };
    let caps = (incoming.capabilities.0, outgoing.capabilities.0);

    // The ping round is ~2 s and this function is called for every Pong. The
    // announcement is idempotent when nothing changed, so only send it on a
    // change (or after a slow refresh window, in case one datagram was lost).
    // Repeating it forever produced ~2 identical AuthRequest/AuthResponse and
    // audit-log records per second per link on the user's live install.
    {
        let mut announced = ctx.auth_announced.borrow_mut();
        if let Some(previous) = announced.get(&addr) {
            if (previous.incoming, previous.outgoing) == caps
                && previous.sent_at.elapsed() < AUTH_REFRESH_INTERVAL
            {
                return;
            }
        }
        announced.insert(
            addr,
            AnnouncedPermissions {
                incoming: caps.0,
                outgoing: caps.1,
                sent_at: Instant::now(),
            },
        );
    }

    let event = ProtoEvent::AuthRequest {
        session_id: auth_session(ctx, addr),
        incoming_caps: caps.0,
        outgoing_caps: caps.1,
    };
    let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
    if let Err(e) = conn.send(&buf[..len]).await {
        // Allow the next Pong to retry.
        ctx.auth_announced.borrow_mut().remove(&addr);
        log::debug!("failed to announce permissions to {addr}: {e}");
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

async fn receive_loop(ctx: ConnCtx, handle: ClientHandle, active: ActiveConnection) {
    let addr = active.addr;
    let mut buf = [0u8; MAX_CLIPBOARD_FRAME];
    loop {
        if !ctx.is_current(handle, active.generation) {
            return;
        }
        let n = match active.conn.recv(&mut buf).await {
            Ok(n) => n,
            Err(_) => break,
        };
        // A replacement transport may have been installed while this one was
        // blocked in `recv`; never let stale data touch the client state.
        if !ctx.is_current(handle, active.generation) {
            return;
        }
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
                        // `active_addr` is owned by the connect path and is
                        // set exactly once per transport. A `Pong` must never
                        // rewrite it: with two transports for the same peer
                        // (legacy happy-eyeballs dialing) this made the UI
                        // subtitle alternate between the IPv4 and the IPv6
                        // address every ping round.
                        ctx.client_manager.set_alive(handle, alive);
                        ctx.client_manager
                            .set_last_pong_at(handle, Some(unix_ms_now()));
                        ctx.ping_response.borrow_mut().insert(addr);
                        if let Some(sent) = ctx.ping_sent.borrow_mut().remove(&addr) {
                            let rtt = sent.elapsed().as_millis().min(u32::MAX as u128) as u32;
                            ctx.client_manager.set_rtt_ms(handle, Some(rtt));
                        }
                        if let Some(capabilities) = capabilities {
                            ctx.client_manager
                                .set_peer_capabilities(handle, capabilities);
                            send_hello_once(&ctx, addr, &active.conn).await;
                            if capabilities & lan_mouse_proto::capabilities::PERMISSIONS != 0 {
                                send_auth_request(&ctx, handle, addr, &active.conn).await;
                            }
                        }
                        ctx.notify_state_changed(handle);
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
                        ctx.notify_state_changed(handle);
                    }
                    ProtoEvent::AuthResponse {
                        incoming_caps,
                        outgoing_caps,
                        ..
                    } => {
                        ctx.client_manager.set_peer_permissions(
                            handle,
                            Some(incoming_caps),
                            Some(outgoing_caps),
                        );
                        ctx.notify_state_changed(handle);
                    }
                    ProtoEvent::AuthRevoke { reason, .. } => {
                        log::warn!(
                            "peer {addr} explicitly disconnected (auth revoke reason {reason}); closing link"
                        );
                        // A user disconnect on either side is mandatory for both
                        // sides: never let the reconnect loop revive the link.
                        ctx.client_manager.deactivate_client(handle);
                        ctx.notify_state_changed(handle);
                        // Close whatever transport is live *now*: a capture
                        // retry racing this frame may already have installed a
                        // replacement generation, which `active.generation`
                        // (the transport this loop was started for) no longer
                        // identifies, so `take_if_current` would silently
                        // leave it alive.
                        if let Some(current) = ctx.active(handle) {
                            ctx.disconnect(handle, current.generation).await;
                        }
                        return;
                    }
                    // The listener is the responder for permission requests.
                    ProtoEvent::AuthRequest { .. } => {}
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
    ctx.disconnect(handle, active.generation).await;
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

    fn sa(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse().expect("ip"), port)
    }

    #[test]
    fn dial_order_keeps_the_remembered_address_first() {
        let v4a = sa("192.168.1.1", 4242);
        let v4b = sa("10.0.0.1", 4242);
        assert_eq!(dial_order([v4a, v4b], Some(v4b)), vec![v4b, v4a]);
    }

    #[test]
    fn dial_order_is_deterministic_and_deduplicated() {
        let a = sa("10.0.0.2", 4242);
        let b = sa("10.0.0.1", 4242);
        assert_eq!(dial_order([a, b, a], None), vec![b, a]);
    }

    #[test]
    fn dial_order_ignores_a_preference_outside_the_candidate_set() {
        let v4 = sa("192.168.1.10", 4242);
        let other = sa("10.0.0.10", 4242);
        assert_eq!(
            dial_order([v4, other], Some(sa("192.168.1.99", 4242))),
            vec![other, v4]
        );
    }

    #[test]
    fn candidates_never_dial_this_machine() {
        let own = std::collections::HashSet::from([
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        ]);
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)), 4242);
        let wrong_self = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), 4242);
        assert_eq!(
            dialable_candidates([wrong_self, peer], &own),
            vec![peer],
            "a saved address that now belongs to this machine must not be dialed"
        );
    }

    #[test]
    fn candidates_that_only_point_at_this_machine_leave_no_dial_target() {
        let own = std::collections::HashSet::from([
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)),
        ]);
        let self_a = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), 4242);
        let self_b = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), 4243);
        assert!(
            dialable_candidates([self_a, self_b], &own).is_empty(),
            "no candidate may be dialed when every saved address belongs to this host"
        );
    }

    #[test]
    fn outgoing_addrs_use_the_incoming_peer_address_when_dns_is_empty() {
        let incoming = sa("192.168.1.42", 54321);
        assert_eq!(
            outgoing_addrs(std::collections::HashSet::new(), 4242, Some(incoming), None),
            vec![sa("192.168.1.42", 4242)],
            "the incoming source address is the only reachable candidate"
        );
    }

    #[test]
    fn outgoing_addrs_prefer_the_incoming_address_over_stale_dns() {
        let incoming = sa("192.168.1.42", 54321);
        let mut ips = std::collections::HashSet::new();
        ips.insert("10.0.0.1".parse::<IpAddr>().expect("ip"));
        assert_eq!(
            outgoing_addrs(ips, 4242, Some(incoming), None),
            vec![sa("192.168.1.42", 4242), sa("10.0.0.1", 4242)]
        );
    }

    #[test]
    fn outgoing_addrs_keep_a_valid_remembered_address_first() {
        let incoming = sa("192.168.1.42", 54321);
        let remembered = sa("10.0.0.1", 4242);
        let mut ips = std::collections::HashSet::new();
        ips.insert("10.0.0.1".parse::<IpAddr>().expect("ip"));
        ips.insert("192.168.1.42".parse::<IpAddr>().expect("ip"));
        assert_eq!(
            outgoing_addrs(ips, 4242, Some(incoming), Some(remembered)),
            vec![remembered, sa("192.168.1.42", 4242)]
        );
    }

    #[test]
    fn outgoing_addrs_ignore_ipv6_incoming_and_empty_sets() {
        let incoming = sa("fd00::42", 54321);
        assert!(
            outgoing_addrs(std::collections::HashSet::new(), 4242, Some(incoming), None).is_empty(),
            "Lan Mouse is IPv4-only"
        );
        assert!(outgoing_addrs(std::collections::HashSet::new(), 4242, None, None).is_empty());
    }

    #[test]
    fn ipv4_only_dial_filters_ipv6_candidates() {
        let v4 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10));
        let v6 = IpAddr::V6(std::net::Ipv6Addr::LOCALHOST);
        let addrs = [v4, v6]
            .into_iter()
            .filter(IpAddr::is_ipv4)
            .map(|ip| SocketAddr::new(ip, 4242))
            .collect::<Vec<_>>();
        assert_eq!(addrs, vec![SocketAddr::new(v4, 4242)]);
    }

    use crate::client::ClientManager;
    use crate::config::ConfigClient;
    use crate::crypto;
    use crate::emulation::{Emulation, EmulationEvent};
    use crate::listen::{LanMouseListener, ListenEvent};
    use futures::StreamExt;
    use lan_mouse_ipc::Position as IpcPosition;
    use lan_mouse_proto::Position as ProtoPosition;
    use std::net::{IpAddr, Ipv4Addr};
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
            fingerprint: None,
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
            Default::default(),
            Arc::new(RwLock::new(true)),
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
                ListenEvent::Closed { .. } => continue,
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
                    "a Hello must never be sent to a peer that did not advertise capabilities"
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
                    None,
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
                let trusted = Arc::new(RwLock::new(vec![crate::trusted::AddressRule::legacy(
                    crate::trusted::TrustedNet::parse("127.0.0.1").expect("cidr"),
                )]));
                let mut listener = LanMouseListener::new(
                    port,
                    listener_cert,
                    authorized.clone(),
                    trusted,
                    Default::default(),
                    Default::default(),
                    Arc::new(RwLock::new(true)),
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
                let pairing: Arc<RwLock<Option<crate::listen::PairingWindow>>> =
                    Arc::new(RwLock::new(Some(crate::listen::PairingWindow {
                        until: Instant::now() + Duration::from_secs(60),
                        incoming: lan_mouse_ipc::DirectionPolicy::allow_all(),
                        outgoing: lan_mouse_ipc::DirectionPolicy::allow_all(),
                    })));
                let mut listener = LanMouseListener::new(
                    port,
                    listener_cert,
                    authorized.clone(),
                    Arc::new(RwLock::new(Vec::new())),
                    pairing,
                    Default::default(),
                    Arc::new(RwLock::new(true)),
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
                        ListenEvent::Msg { .. }
                        | ListenEvent::Clipboard { .. }
                        | ListenEvent::Closed { .. } => {}
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
                    None,
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
                    None,
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

    /// Full pipeline E2E test: [`crate::capture::Capture`] (using `DummyInputCapture`)
    /// -> [`LanMouseConnection`] (DTLS) -> [`LanMouseListener`] -> [`Emulation`]
    /// (using `DummyEmulation`) + clipboard frame delivery over the same DTLS
    /// session, followed by explicit release and `Leave`.
    #[tokio::test(flavor = "current_thread")]
    async fn full_capture_to_emulation_and_clipboard_pipeline_e2e() {
        use crate::capture::{Capture, CaptureType, ICaptureEvent};
        use lan_mouse_proto::clipboard::ClipboardFrame;

        let local = LocalSet::new();
        local
            .run_until(async {
                let port_b = free_udp_port();
                let cert_a = unique_cert("e2e-a");
                let cert_b = unique_cert("e2e-b");
                let fp_a = crypto::certificate_fingerprint(&cert_a);

                // Peer B: Listener + Emulation (Dummy) + Clipboard receiver
                let listener_b = listener_with_authorized(port_b, cert_b, vec![fp_a.clone()]).await;
                let (clipboard_b_tx, mut clipboard_b_rx) = local_channel::mpsc::channel();
                let mut emulation_b = Emulation::new(
                    Some(input_emulation::Backend::Dummy),
                    listener_b,
                    clipboard_b_tx,
                    None,
                );
                // Start B's backend before A connects. In production the
                // service does this on `Connected`; here the test has no
                // service, and a lazy backend would advertise `alive = false`
                // in its first Pong, making A's Enter send fail with
                // `TargetEmulationDisabled` (the old flaky failure).
                emulation_b.warm_up();
                let warmed = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if let EmulationEvent::EmulationEnabled = emulation_b.event().await {
                            break;
                        }
                    }
                })
                .await;
                assert!(warmed.is_ok(), "B's emulation backend must warm up");

                // Peer A: ClientManager + LanMouseConnection + Capture (DummyInputCapture emits on Position::Left)
                let manager_a = ClientManager::default();
                let handle_a = manager_a.add_with_config(ConfigClient {
                    ips: HashSet::from([IpAddr::V4(Ipv4Addr::LOCALHOST)]),
                    hostname: None,
                    port: port_b,
                    pos: IpcPosition::Left,
                    active: true,
                    enter_hook: None,
                    leave_hook: None,
                    sensitivity: 100,
                    fingerprint: None,
                });
                let conn_a = LanMouseConnection::new(cert_a, manager_a.clone(), 2000);
                let clipboard_sender_a = conn_a.sender();

                // Create the capture task first (it polls the connection), then
                // wait for the application-level Hello handshake before asking
                // the Dummy backend to emit Begin. Creating the capture later
                // avoids racing the DTLS/Hello handshake under a loaded parallel
                // run, which used to lose the first Enter and release the grab
                // before the peer was ready.
                let mut capture_a =
                    Capture::new(Some(input_capture::Backend::Dummy), conn_a, vec![], 0, 2000);
                capture_a.connect(handle_a);
                let link_deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    let ready = manager_a
                        .get_state(handle_a)
                        .is_some_and(|(_, state)| state.peer_commit.is_some() && state.alive);
                    if ready || Instant::now() >= link_deadline {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert!(
                    manager_a
                        .get_state(handle_a)
                        .is_some_and(|(_, state)| { state.peer_commit.is_some() && state.alive }),
                    "A must finish the Hello handshake and see B's emulation alive before Begin"
                );
                capture_a.create(handle_a, IpcPosition::Left, CaptureType::Default);

                // 1. DummyInputCapture fires Begin -> Capture sends Enter -> Emulation B emits Entered
                let mut a_entered = false;
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline && !a_entered {
                    if let Ok(ICaptureEvent::ClientEntered(h)) =
                        tokio::time::timeout(Duration::from_millis(250), capture_a.event()).await
                    {
                        assert_eq!(h, handle_a);
                        a_entered = true;
                    }
                }
                assert!(a_entered, "Capture A must report ClientEntered");

                // Fresh deadline: under a loaded parallel test run the DTLS
                // handshake can consume most of the first deadline, and the
                // listener must not be starved of its own wait window.
                let b_deadline = Instant::now() + Duration::from_secs(10);
                let mut b_entered = false;
                while Instant::now() < b_deadline && !b_entered {
                    if let Ok(EmulationEvent::Entered {
                        pos, fingerprint, ..
                    }) =
                        tokio::time::timeout(Duration::from_millis(250), emulation_b.event()).await
                    {
                        // Left on A enters from the Right on B
                        assert_eq!(pos, IpcPosition::Right);
                        assert_eq!(fingerprint, fp_a);
                        b_entered = true;
                    }
                }
                assert!(
                    b_entered,
                    "Emulation B must receive Entered from A; A state={:?}",
                    manager_a.get_state(handle_a)
                );

                // 2. Send a ClipboardFrame over the same established DTLS connection
                let offer = ClipboardFrame::Offer {
                    serial: 77,
                    origin: 12345,
                    hash: [0xAB; 32],
                    items: vec![(lan_mouse_proto::clipboard::MIME_TEXT, 11)],
                };
                clipboard_sender_a
                    .send_raw(handle_a, &offer.encode())
                    .await
                    .expect("send clipboard frame");

                let (_, received_frame) =
                    tokio::time::timeout(Duration::from_secs(5), clipboard_b_rx.recv())
                        .await
                        .expect("timed out waiting for clipboard frame on B")
                        .expect("clipboard channel open");
                assert_eq!(received_frame, offer);

                // 3. Release Capture A -> must emit ClientLeft
                capture_a.release();
                let mut a_left = false;
                while Instant::now() < deadline && !a_left {
                    if let Ok(ICaptureEvent::ClientLeft(h)) =
                        tokio::time::timeout(Duration::from_millis(250), capture_a.event()).await
                    {
                        assert_eq!(h, handle_a);
                        a_left = true;
                    }
                }
                assert!(a_left, "Capture A must report ClientLeft on release");

                capture_a.terminate().await;
                emulation_b.terminate().await;
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn state_rx_notifies_when_connection_succeeds() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let cert_a = unique_cert("state-rx-a");
                let cert_b = unique_cert("state-rx-b");
                let fp_a = crypto::certificate_fingerprint(&cert_a);
                let mut listener_b = listener_with_authorized(port, cert_b, vec![fp_a]).await;

                let (manager_a, handle_a) = client_manager_for(port);
                let mut conn_a = LanMouseConnection::new(cert_a, manager_a.clone(), 2000);
                let mut state_rx = conn_a.take_state_rx().expect("state_rx");

                conn_a.ensure_connected(handle_a);

                let notified = tokio::time::timeout(Duration::from_secs(5), state_rx.recv())
                    .await
                    .expect("timed out waiting for state_rx notification")
                    .expect("state_rx open");
                assert_eq!(notified, handle_a);

                let (_, state) = manager_a.get_state(handle_a).expect("client state");
                assert!(state.active_addr.is_some(), "active_addr must be set");
                assert!(state.alive, "alive must be set on connect");

                listener_b.terminate().await;
            })
            .await;
    }

    /// A client without a pinned certificate learns the peer fingerprint on
    /// the first successful connection (TOFU), so later connections can be
    /// authenticated instead of blindly trusted.
    #[tokio::test(flavor = "current_thread")]
    async fn outgoing_connection_learns_the_server_fingerprint() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("tofu-listener");
                let client_cert = unique_cert("tofu-client");
                let listener_fp = crypto::certificate_fingerprint(&listener_cert);
                let client_fp = crypto::certificate_fingerprint(&client_cert);
                let mut listener =
                    listener_with_authorized(port, listener_cert, vec![client_fp]).await;

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

                let deadline = Instant::now() + Duration::from_secs(5);
                while manager.fingerprint(handle).is_none() && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                assert_eq!(
                    manager.fingerprint(handle).as_deref(),
                    Some(listener_fp.as_str()),
                    "the listening certificate must be pinned after the handshake"
                );

                listener.terminate().await;
            })
            .await;
    }

    #[test]
    fn pin_decision_distinguishes_match_rotation_and_impersonation() {
        assert_eq!(pin_decision("aa", "aa", false), PinDecision::Match);
        assert_eq!(pin_decision("AA", "aa", false), PinDecision::Match);
        assert_eq!(pin_decision("aa", "bb", true), PinDecision::RotateKnown);
        assert_eq!(pin_decision("aa", "bb", false), PinDecision::Reject);
    }

    /// With a pinned fingerprint the dialer must refuse a server that
    /// presents a different certificate, even if it completes the DTLS
    /// handshake (impersonation at a reused address).
    #[tokio::test(flavor = "current_thread")]
    async fn outgoing_connection_rejects_a_mismatched_pin() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("pin-listener");
                let client_cert = unique_cert("pin-client");
                let client_fp = crypto::certificate_fingerprint(&client_cert);
                let mut listener =
                    listener_with_authorized(port, listener_cert, vec![client_fp]).await;

                let (manager, handle) = client_manager_for(port);
                // Pin the wrong certificate (as if the IP now belongs to a
                // different machine).
                manager.set_fingerprint(
                    handle,
                    Some(crypto::certificate_fingerprint(&unique_cert("impostor"))),
                );
                let conn = LanMouseConnection::new(client_cert, manager.clone(), 500);
                let result = conn
                    .send(
                        ProtoEvent::Enter {
                            pos: ProtoPosition::Right,
                            serial: 1,
                        },
                        handle,
                    )
                    .await;
                assert!(result.is_err(), "mismatched server certificate must fail");
                assert!(
                    manager.active_addr(handle).is_none(),
                    "no connection may be established"
                );

                listener.terminate().await;
            })
            .await;
    }

    /// A revoked fingerprint wins over the authorized map, the address
    /// allowlist and an open pairing window.
    #[tokio::test(flavor = "current_thread")]
    async fn denied_fingerprint_is_rejected_before_anything_else() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("deny-listener");
                let client_cert = unique_cert("deny-client");
                let client_fp = crypto::certificate_fingerprint(&client_cert);

                let authorized = Arc::new(RwLock::new(HashMap::from([(
                    client_fp.clone(),
                    "peer".to_string(),
                )])));
                let mut deny_store = crate::devices::DeviceStore::default();
                deny_store.deny_until(client_fp, None, Some("test".to_string()));
                let denied = Arc::new(RwLock::new(deny_store));
                let mut listener = LanMouseListener::new(
                    port,
                    listener_cert,
                    authorized,
                    Arc::new(RwLock::new(vec![crate::trusted::AddressRule::legacy(
                        crate::trusted::TrustedNet::parse("127.0.0.1").expect("cidr"),
                    )])),
                    Arc::new(RwLock::new(Some(crate::listen::PairingWindow {
                        until: Instant::now() + Duration::from_secs(60),
                        incoming: lan_mouse_ipc::DirectionPolicy::allow_all(),
                        outgoing: lan_mouse_ipc::DirectionPolicy::allow_all(),
                    }))),
                    denied,
                    Arc::new(RwLock::new(true)),
                )
                .await
                .expect("listener");

                let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
                let result = connect(addr, client_cert, Duration::from_secs(2), None).await;
                assert!(result.is_err(), "denied device must not connect");

                listener.terminate().await;
            })
            .await;
    }

    /// Pairing is single use: accepting one device closes the window, so a
    /// second unknown device cannot ride along.
    #[tokio::test(flavor = "current_thread")]
    async fn pairing_window_accepts_only_one_device() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("pairing-once-listener");
                let first_cert = unique_cert("pairing-first");
                let second_cert = unique_cert("pairing-second");
                let pairing: Arc<RwLock<Option<crate::listen::PairingWindow>>> =
                    Arc::new(RwLock::new(Some(crate::listen::PairingWindow {
                        until: Instant::now() + Duration::from_secs(60),
                        incoming: lan_mouse_ipc::DirectionPolicy::allow_all(),
                        outgoing: lan_mouse_ipc::DirectionPolicy::allow_all(),
                    })));
                let mut listener = LanMouseListener::new(
                    port,
                    listener_cert,
                    Arc::new(RwLock::new(HashMap::new())),
                    Arc::new(RwLock::new(Vec::new())),
                    pairing.clone(),
                    Default::default(),
                    Arc::new(RwLock::new(true)),
                )
                .await
                .expect("listener");

                let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
                let first = connect(addr, first_cert, Duration::from_secs(2), None).await;
                assert!(first.is_ok(), "first device may pair");
                let deadline = Instant::now() + Duration::from_secs(2);
                while pairing.read().expect("lock").is_some() && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                assert!(
                    pairing.read().expect("lock").is_none(),
                    "pairing window must close after the first device"
                );

                let second = connect(addr, second_cert, Duration::from_secs(2), None).await;
                assert!(second.is_err(), "second device must be rejected");

                listener.terminate().await;
            })
            .await;
    }

    /// Disconnecting a revoked session closes the live DTLS connection
    /// instead of leaving it able to inject input until it drops.
    #[tokio::test(flavor = "current_thread")]
    async fn disconnect_closes_a_live_incoming_session() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("revoke-listener");
                let client_cert = unique_cert("revoke-client");
                let client_fp = crypto::certificate_fingerprint(&client_cert);
                let mut listener =
                    listener_with_authorized(port, listener_cert, vec![client_fp]).await;

                let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
                let (conn, _) = connect(addr, client_cert, Duration::from_secs(2), None)
                    .await
                    .expect("connection");
                // Wait until the listener registered the session, otherwise
                // the disconnect races with the accept task. The event carries
                // the peer address, not the listener's.
                let peer = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        match listener.next().await {
                            Some(ListenEvent::Accept { addr: accepted, .. }) => break accepted,
                            Some(_) => continue,
                            None => panic!("listener closed before accepting"),
                        }
                    }
                })
                .await
                .expect("timed out waiting for accept");

                listener.disconnect(peer).await;

                // The listener must report the session as closed.
                let closed = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        match listener.next().await {
                            Some(ListenEvent::Closed { addr: closed }) if closed == peer => {
                                break true;
                            }
                            Some(_) => continue,
                            None => break false,
                        }
                    }
                })
                .await
                .expect("timed out waiting for close");
                assert!(closed, "listener stream ended unexpectedly");

                // The session is gone from the listener's connection list, so
                // revoking it again is a harmless no-op.
                listener.disconnect(peer).await;
                let _ = conn;

                listener.terminate().await;
            })
            .await;
    }

    /// The same peer (certificate) connecting again — over the other address
    /// family, or after a network flap that left the old transport half-open —
    /// must replace the transport instead of becoming a second device with a
    /// second address.
    #[tokio::test(flavor = "current_thread")]
    async fn reconnect_with_same_certificate_replaces_the_session() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("dedupe-listener");
                let client_cert = unique_cert("dedupe-client");
                let fp = crypto::certificate_fingerprint(&client_cert);
                let mut listener = listener_with_authorized(port, listener_cert, vec![fp]).await;

                // Keep the first transport alive on purpose: the replacement
                // must close it, not depend on the test dropping it.
                let first = tokio::time::timeout(
                    Duration::from_secs(15),
                    raw_dtls_client(port, client_cert.clone()),
                )
                .await
                .expect("timed out waiting for the first handshake");
                let logical = tokio::time::timeout(Duration::from_secs(15), async {
                    loop {
                        match listener.next().await {
                            Some(ListenEvent::Accept { addr, .. }) => break addr,
                            Some(_) => continue,
                            None => panic!("listener closed before accepting"),
                        }
                    }
                })
                .await
                .expect("timed out waiting for the first accept");

                // Second transport from the same identity (new source port,
                // as if the peer switched address family or reconnected after
                // a lost link).
                let second = tokio::time::timeout(
                    Duration::from_secs(15),
                    raw_dtls_client(port, client_cert),
                )
                .await
                .expect("timed out waiting for the replacement handshake");

                // The replacement transport serves the same logical session:
                // events from it arrive under the address the service already
                // knows (no family flip). Event-driven with a generous deadline
                // on purpose: a fixed sleep here made the test sensitive to a
                // loaded CI runner, and the Enter is re-sent if a loopback
                // datagram is dropped.
                let enter = ProtoEvent::Enter {
                    pos: ProtoPosition::Right,
                    serial: 5,
                };
                raw_send(&second, enter).await;
                let deadline = Instant::now() + Duration::from_secs(15);
                let msg_addr = loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    assert!(
                        !remaining.is_zero(),
                        "timed out waiting for the enter from the replaced transport"
                    );
                    match tokio::time::timeout(
                        remaining.min(Duration::from_millis(250)),
                        listener.next(),
                    )
                    .await
                    {
                        Ok(Some(ListenEvent::Msg {
                            event: ProtoEvent::Enter { serial: 5, .. },
                            addr,
                        })) => break addr,
                        Ok(Some(ListenEvent::Accept { addr, .. })) => {
                            panic!("the same peer was accepted twice (second at {addr})")
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => panic!("listener closed while waiting for the enter"),
                        // No event within the tick: the datagram may have been
                        // dropped, retransmit like the real capture does.
                        Err(_) => raw_send(&second, enter).await,
                    }
                };
                assert_eq!(
                    msg_addr, logical,
                    "logical address must stay stable across transport replacement"
                );

                let _ = first;
                listener.terminate().await;
            })
            .await;
    }

    /// The listener enforces the incoming capability matrix per event: a
    /// device granted only the keyboard cannot move the pointer or take the
    /// capture, while its keyboard events still reach the service.
    #[tokio::test(flavor = "current_thread")]
    async fn incoming_capabilities_filter_peer_events() {
        use crate::devices::{Device, DeviceSource, DeviceStore};
        use lan_mouse_ipc::{Capabilities, DirectionPolicy};

        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("caps-filter-listener");
                let client_cert = unique_cert("caps-filter-client");
                let client_fp = crypto::certificate_fingerprint(&client_cert);

                let mut store = DeviceStore::default();
                store.put(Device {
                    fingerprint: client_fp.clone(),
                    name: "keyboard-only".into(),
                    incoming: DirectionPolicy {
                        allowed: true,
                        capabilities: Capabilities::KEYBOARD,
                    },
                    outgoing: DirectionPolicy::deny(),
                    source: DeviceSource::Manual,
                });
                let devices = Arc::new(RwLock::new(store));
                let mut listener = LanMouseListener::new(
                    port,
                    listener_cert,
                    Arc::new(RwLock::new(HashMap::from([(
                        client_fp,
                        "keyboard-only".to_string(),
                    )]))),
                    Arc::new(RwLock::new(Vec::new())),
                    Default::default(),
                    devices,
                    Arc::new(RwLock::new(true)),
                )
                .await
                .expect("listener");

                let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
                let (conn, _) = connect(addr, client_cert, Duration::from_secs(2), None)
                    .await
                    .expect("connection");

                async fn send(conn: &ArcConn, event: ProtoEvent) {
                    let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
                    conn.send(&buf[..len]).await.expect("send");
                }

                // Pointer motion is not granted and must be dropped.
                send(
                    &conn,
                    ProtoEvent::Input(input_event::Event::Pointer(
                        input_event::PointerEvent::Motion {
                            time: 0,
                            dx: 4.0,
                            dy: 2.0,
                        },
                    )),
                )
                .await;
                // Keyboard is granted and must be forwarded.
                send(
                    &conn,
                    ProtoEvent::Input(input_event::Event::Keyboard(
                        input_event::KeyboardEvent::Key {
                            time: 0,
                            key: 30,
                            state: 1,
                        },
                    )),
                )
                .await;

                let event = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        match listener.next().await {
                            Some(ListenEvent::Msg { event, .. }) => break event,
                            Some(_) => continue,
                            None => panic!("listener closed"),
                        }
                    }
                })
                .await
                .expect("timed out waiting for the keyboard event");
                assert!(
                    matches!(event, ProtoEvent::Input(input_event::Event::Keyboard(_))),
                    "expected the keyboard event, got {event:?}"
                );

                // The pointer event must not show up behind it.
                let unexpected = tokio::time::timeout(Duration::from_millis(300), async {
                    loop {
                        match listener.next().await {
                            Some(ListenEvent::Msg { event, .. }) => break Some(event),
                            Some(_) => continue,
                            None => break None,
                        }
                    }
                })
                .await;
                if let Ok(Some(event)) = unexpected {
                    assert!(
                        !matches!(event, ProtoEvent::Input(input_event::Event::Pointer(_))),
                        "pointer input must be filtered out"
                    );
                }

                listener.terminate().await;
            })
            .await;
    }

    /// An address rule carries the permissions of the devices it adopts; the
    /// listener reports them so the service can register a restricted device
    /// instead of granting full access.
    #[tokio::test(flavor = "current_thread")]
    async fn address_rule_permissions_reach_the_auto_authorization() {
        use lan_mouse_ipc::{Capabilities, DirectionPolicy};

        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("rule-listener");
                let client_cert = unique_cert("rule-client");
                let incoming = DirectionPolicy {
                    allowed: true,
                    capabilities: Capabilities::CLIPBOARD,
                };
                let outgoing = DirectionPolicy::deny();
                let rule = crate::trusted::AddressRule::new(
                    crate::trusted::TrustedNet::parse("127.0.0.1").expect("cidr"),
                    incoming,
                    outgoing,
                );
                let mut listener = LanMouseListener::new(
                    port,
                    listener_cert,
                    Arc::new(RwLock::new(HashMap::new())),
                    Arc::new(RwLock::new(vec![rule])),
                    Default::default(),
                    Default::default(),
                    Arc::new(RwLock::new(true)),
                )
                .await
                .expect("listener");

                let (manager, handle) = client_manager_for(port);
                let conn = LanMouseConnection::new(client_cert, manager, 2000);
                conn.send(
                    ProtoEvent::Enter {
                        pos: ProtoPosition::Right,
                        serial: 2,
                    },
                    handle,
                )
                .await
                .expect("connect");

                let (granted_in, granted_out) =
                    tokio::time::timeout(Duration::from_secs(5), async {
                        loop {
                            match listener.next().await {
                                Some(ListenEvent::AutoAuthorized {
                                    incoming, outgoing, ..
                                }) => break (incoming, outgoing),
                                Some(_) => continue,
                                None => panic!("listener closed"),
                            }
                        }
                    })
                    .await
                    .expect("timed out waiting for the auto authorization");
                assert_eq!(granted_in, incoming, "rule incoming policy is reported");
                assert_eq!(granted_out, outgoing, "rule outgoing policy is reported");

                listener.terminate().await;
            })
            .await;
    }

    /// The pairing window grants exactly the permissions configured when it
    /// was opened, reported to the service through AutoAuthorized.
    #[tokio::test(flavor = "current_thread")]
    async fn scoped_pairing_grants_only_the_configured_capabilities() {
        use lan_mouse_ipc::{Capabilities, DirectionPolicy};

        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("scoped-pairing-listener");
                let client_cert = unique_cert("scoped-pairing-client");
                let incoming = DirectionPolicy {
                    allowed: true,
                    capabilities: Capabilities::CLIPBOARD,
                };
                let outgoing = DirectionPolicy::deny();
                let pairing = Arc::new(RwLock::new(Some(crate::listen::PairingWindow {
                    until: Instant::now() + Duration::from_secs(60),
                    incoming,
                    outgoing,
                })));
                let mut listener = LanMouseListener::new(
                    port,
                    listener_cert,
                    Arc::new(RwLock::new(HashMap::new())),
                    Arc::new(RwLock::new(Vec::new())),
                    pairing.clone(),
                    Default::default(),
                    Arc::new(RwLock::new(true)),
                )
                .await
                .expect("listener");

                let (manager, handle) = client_manager_for(port);
                let conn = LanMouseConnection::new(client_cert, manager, 2000);
                conn.send(
                    ProtoEvent::Enter {
                        pos: ProtoPosition::Right,
                        serial: 4,
                    },
                    handle,
                )
                .await
                .expect("pairing accepts the peer");

                let (granted_in, granted_out, from_pairing) =
                    tokio::time::timeout(Duration::from_secs(5), async {
                        loop {
                            match listener.next().await {
                                Some(ListenEvent::AutoAuthorized {
                                    incoming,
                                    outgoing,
                                    from_pairing,
                                    ..
                                }) => break (incoming, outgoing, from_pairing),
                                Some(_) => continue,
                                None => panic!("listener closed"),
                            }
                        }
                    })
                    .await
                    .expect("timed out waiting for the pairing authorization");
                assert!(from_pairing);
                assert_eq!(granted_in, incoming);
                assert_eq!(granted_out, outgoing);
                assert!(
                    pairing.read().expect("lock").is_none(),
                    "the single-use pairing window closes after the accept"
                );

                listener.terminate().await;
            })
            .await;
    }

    /// Two peers that advertise `PERMISSIONS` exchange their grants: the
    /// listener replies to our AuthRequest with its local policy and reports
    /// the peer's grant upward, while we record the listener's grant.
    #[tokio::test(flavor = "current_thread")]
    async fn permission_negotiation_exchanges_grants() {
        use crate::devices::{Device, DeviceSource, DeviceStore};
        use lan_mouse_ipc::{Capabilities, DirectionPolicy};

        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("auth-listener");
                let client_cert = unique_cert("auth-client");
                let client_fp = crypto::certificate_fingerprint(&client_cert);
                let listener_fp = crypto::certificate_fingerprint(&listener_cert);

                let mut listener_store = DeviceStore::default();
                listener_store.put(Device {
                    fingerprint: client_fp.clone(),
                    name: "client".into(),
                    incoming: DirectionPolicy {
                        allowed: true,
                        capabilities: Capabilities::KEYBOARD,
                    },
                    outgoing: DirectionPolicy {
                        allowed: true,
                        capabilities: Capabilities::CLIPBOARD,
                    },
                    source: DeviceSource::Manual,
                });
                let listener_devices = Arc::new(RwLock::new(listener_store));

                let listener =
                    listener_with_authorized(port, listener_cert, vec![client_fp.clone()]).await;
                let (clipboard_tx, _clipboard_rx) = local_channel::mpsc::channel();
                let mut emulation = Emulation::new(
                    Some(input_emulation::Backend::Dummy),
                    listener,
                    clipboard_tx,
                    Some(listener_devices),
                );

                let (manager, handle) = client_manager_for(port);
                manager.set_fingerprint(handle, Some(listener_fp.clone()));
                let mut client_store = DeviceStore::default();
                client_store.put(Device {
                    fingerprint: listener_fp,
                    name: "listener".into(),
                    incoming: DirectionPolicy {
                        allowed: true,
                        capabilities: Capabilities::POINTER,
                    },
                    outgoing: DirectionPolicy {
                        allowed: true,
                        capabilities: Capabilities::KEYBOARD,
                    },
                    source: DeviceSource::Manual,
                });
                let conn = LanMouseConnection::new(client_cert, manager.clone(), 2000);
                let mut conn = conn;
                conn.set_device_store(Arc::new(RwLock::new(client_store)));
                conn.send(
                    ProtoEvent::Enter {
                        pos: ProtoPosition::Right,
                        serial: 9,
                    },
                    handle,
                )
                .await
                .expect("connect");

                // We receive the listener's grant: incoming = KEYBOARD (2),
                // outgoing = CLIPBOARD (4).
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut granted = None;
                while Instant::now() < deadline && granted.is_none() {
                    if let Some((_, state)) = manager.get_state(handle) {
                        if state.peer_incoming_caps.is_some() {
                            granted = Some((state.peer_incoming_caps, state.peer_outgoing_caps));
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                assert_eq!(
                    granted,
                    Some((
                        Some(Capabilities::KEYBOARD.0),
                        Some(Capabilities::CLIPBOARD.0)
                    )),
                    "the listener grant must be recorded on the client"
                );

                // The listener reports our grant upward: incoming = POINTER (1),
                // outgoing = KEYBOARD (2).
                let event = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        match emulation.event().await {
                            EmulationEvent::PeerPermissions {
                                incoming_caps,
                                outgoing_caps,
                                fingerprint,
                                ..
                            } => break (incoming_caps, outgoing_caps, fingerprint),
                            _ => continue,
                        }
                    }
                })
                .await
                .expect("timed out waiting for the peer permissions event");
                assert_eq!(event.0, Capabilities::POINTER.0);
                assert_eq!(event.1, Capabilities::KEYBOARD.0);
                assert_eq!(event.2, client_fp);
            })
            .await;
    }

    /// Regression: a saved client whose pinned certificate no longer matches
    /// the peer (the peer was reinstalled/re-paired) used to fail every
    /// Connect until the user deleted and re-added the device. If the new
    /// certificate is already a known/authorized device, the stale pin must be
    /// refreshed and the connection allowed.
    #[tokio::test(flavor = "current_thread")]
    async fn a_saved_client_with_a_rotated_certificate_repins_a_known_device() {
        use crate::devices::{Device, DeviceSource, DeviceStore};
        use std::sync::RwLock;

        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("rotation-listener");
                let actual_fp = crypto::certificate_fingerprint(&listener_cert);
                let client_cert = unique_cert("rotation-client");
                let client_fp = crypto::certificate_fingerprint(&client_cert);
                let stale_fp = crypto::certificate_fingerprint(&unique_cert("rotation-stale"));

                let listener = listener_with_authorized(port, listener_cert, vec![client_fp]).await;
                let (clipboard_tx, _clipboard_rx) = local_channel::mpsc::channel();
                let mut emulation = Emulation::new(
                    Some(input_emulation::Backend::Dummy),
                    listener,
                    clipboard_tx,
                    None,
                );

                let (manager, handle) = client_manager_for(port);
                // The config has a stale pin from before the peer's identity
                // changed.
                manager.set_fingerprint(handle, Some(stale_fp));

                let mut devices = DeviceStore::default();
                devices.put(Device {
                    fingerprint: actual_fp.clone(),
                    name: "peer".into(),
                    incoming: lan_mouse_ipc::DirectionPolicy::allow_all(),
                    outgoing: lan_mouse_ipc::DirectionPolicy::allow_all(),
                    source: DeviceSource::Manual,
                });
                let mut conn = LanMouseConnection::new(client_cert, manager.clone(), 3000);
                conn.set_device_store(Arc::new(RwLock::new(devices)));
                conn.ensure_connected(handle);

                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline
                    && (manager.active_addr(handle).is_none()
                        || manager.fingerprint(handle).as_deref() != Some(actual_fp.as_str()))
                {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }

                assert_eq!(
                    manager.fingerprint(handle),
                    Some(actual_fp.clone()),
                    "the stale pin must be refreshed to the known peer certificate"
                );
                assert!(
                    manager.active_addr(handle).is_some(),
                    "the saved client must reconnect after the pin refresh"
                );

                emulation.terminate().await;
            })
            .await;
    }

    /// The permission announcement is edge-triggered: it used to be sent for
    /// every received Pong (~4 per 2 s ping round), producing an identical
    /// AuthRequest/AuthResponse/PeerPermissions cycle and an audit-log record
    /// forever. Once announced, unchanged bits must stay quiet.
    #[tokio::test(flavor = "current_thread")]
    async fn permission_announcement_is_not_repeated_on_every_pong() {
        use crate::devices::DeviceStore;
        use std::sync::RwLock;

        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("auth-dedupe-listener");
                let client_cert = unique_cert("auth-dedupe-client");
                let fp = crypto::certificate_fingerprint(&client_cert);
                let listener = listener_with_authorized(port, listener_cert, vec![fp]).await;
                let (clipboard_tx, _clipboard_rx) = local_channel::mpsc::channel();
                let mut emulation = Emulation::new(
                    Some(input_emulation::Backend::Dummy),
                    listener,
                    clipboard_tx,
                    None,
                );

                let (manager, handle) = client_manager_for(port);
                let mut conn = LanMouseConnection::new(client_cert, manager.clone(), 2000);
                conn.set_device_store(Arc::new(RwLock::new(DeviceStore::default())));
                conn.ensure_connected(handle);

                // The first Pong makes the client announce its grants.
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut announced = 0usize;
                while Instant::now() < deadline && announced == 0 {
                    if let Ok(EmulationEvent::PeerPermissions { .. }) =
                        tokio::time::timeout(Duration::from_millis(250), emulation.event()).await
                    {
                        announced += 1;
                    }
                }
                assert_eq!(
                    announced, 1,
                    "the client must announce its permissions once"
                );

                // Several more ping rounds: no repeated announcement.
                let quiet_until = Instant::now() + Duration::from_millis(2500);
                while Instant::now() < quiet_until {
                    if let Ok(EmulationEvent::PeerPermissions { .. }) =
                        tokio::time::timeout(Duration::from_millis(100), emulation.event()).await
                    {
                        announced += 1;
                    }
                }
                assert_eq!(
                    announced, 1,
                    "unchanged permission bits must not be re-announced every Pong"
                );

                emulation.terminate().await;
            })
            .await;
    }

    /// The pairing window is single use: two unknown devices racing for it must
    /// not both be authorized (the verify callback can see it open for both
    /// concurrent handshakes; the consumption must be atomic).
    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_pairing_adopts_at_most_one_device() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("pairing-race-listener");
                let authorized: Arc<RwLock<HashMap<String, String>>> =
                    Arc::new(RwLock::new(HashMap::new()));
                let pairing: Arc<RwLock<Option<crate::listen::PairingWindow>>> =
                    Arc::new(RwLock::new(Some(crate::listen::PairingWindow {
                        until: Instant::now() + Duration::from_secs(60),
                        incoming: lan_mouse_ipc::DirectionPolicy::allow_all(),
                        outgoing: lan_mouse_ipc::DirectionPolicy::allow_all(),
                    })));
                let mut listener = LanMouseListener::new(
                    port,
                    listener_cert,
                    authorized.clone(),
                    Arc::new(RwLock::new(Vec::new())),
                    pairing,
                    Default::default(),
                    Arc::new(RwLock::new(true)),
                )
                .await
                .expect("listener");

                let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
                let first = connect(
                    addr,
                    unique_cert("pairing-race-a"),
                    Duration::from_secs(3),
                    None,
                );
                let second = connect(
                    addr,
                    unique_cert("pairing-race-b"),
                    Duration::from_secs(3),
                    None,
                );
                let _ = tokio::join!(first, second);
                // Give the accept tasks time to finish consuming/rejecting.
                tokio::time::sleep(Duration::from_millis(200)).await;

                assert!(
                    authorized.read().expect("lock").len() <= 1,
                    "a single-use pairing window adopted more than one device"
                );

                listener.terminate().await;
            })
            .await;
    }

    /// A silent incoming session must be closed after the emulation timeout.
    /// Before this, dead DTLS sessions were never closed and accumulated in the
    /// listener until its live-connection cap rejected every new incoming peer
    /// (only a service restart recovered).
    #[tokio::test(flavor = "current_thread")]
    async fn silent_incoming_session_is_closed_after_the_timeout() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("silent-timeout-listener");
                let client_cert = unique_cert("silent-timeout-client");
                let fp = crypto::certificate_fingerprint(&client_cert);
                let listener = listener_with_authorized(port, listener_cert, vec![fp]).await;
                let (clipboard_tx, _clipboard_rx) = local_channel::mpsc::channel();
                let mut emulation = Emulation::new(
                    Some(input_emulation::Backend::Dummy),
                    listener,
                    clipboard_tx,
                    None,
                );

                // Handshake only: the peer never sends an event afterwards.
                let client = raw_dtls_client(port, client_cert).await;
                let start = Instant::now();
                let disconnected = tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if let EmulationEvent::Disconnected { .. } = emulation.event().await {
                            break;
                        }
                    }
                })
                .await;
                assert!(
                    disconnected.is_ok(),
                    "a silent incoming session must be closed (was kept until restart)"
                );
                assert!(
                    start.elapsed() >= Duration::from_secs(2),
                    "the session must not be closed immediately ({:?})",
                    start.elapsed()
                );

                let _ = client;
                emulation.terminate().await;
            })
            .await;
    }
}
