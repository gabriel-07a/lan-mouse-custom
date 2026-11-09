use crate::config::local_commit;
use crate::devices::DeviceStore;
#[cfg(not(target_os = "windows"))]
use crate::edge_return::EdgeReturnTracker;
use crate::listen::{LanMouseListener, ListenEvent, ListenerCommand, ListenerCreationError};
use futures::StreamExt;
use input_emulation::{EmulationHandle, InputEmulation, InputEmulationError, PointerEdge};
use input_event::Event;
use lan_mouse_proto::clipboard::ClipboardFrame;
use lan_mouse_proto::{CAPABILITIES, PROTO_VERSION, Position, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::Cell,
    collections::HashMap,
    net::SocketAddr,
    rc::Rc,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tokio::{
    select,
    sync::mpsc::UnboundedReceiver,
    task::{JoinHandle, spawn_local},
};

/// How long the listening side tolerates silence (no ping, no input) before
/// it releases held keys and tears the emulation handle down. Wide enough
/// to survive Wi-Fi jitter and a couple of lost pings.
const EMULATION_TIMEOUT: Duration = Duration::from_secs(3);

/// emulation handling events received from a listener
pub(crate) struct Emulation {
    task: JoinHandle<()>,
    request_tx: Sender<EmulationRequest>,
    event_rx: Receiver<EmulationEvent>,
    selected_backend: Rc<Cell<Option<input_emulation::Backend>>>,
    reply_tx: Sender<ListenerCommand>,
}

pub(crate) enum EmulationEvent {
    Connected {
        addr: SocketAddr,
        fingerprint: String,
    },
    /// a peer was auto-authorized (address rule or pairing window)
    AutoAuthorized {
        addr: SocketAddr,
        fingerprint: String,
        /// the pairing window accepted this peer (single use)
        from_pairing: bool,
        /// permissions granted by the address rule that adopted the device
        incoming: lan_mouse_ipc::DirectionPolicy,
        outgoing: lan_mouse_ipc::DirectionPolicy,
    },
    ConnectionAttempt {
        addr: SocketAddr,
        fingerprint: String,
    },
    /// new connection
    Entered {
        /// address of the connection
        addr: SocketAddr,
        /// position of the connection
        pos: lan_mouse_ipc::Position,
        /// certificate fingerprint of the connection
        fingerprint: String,
    },
    /// connection closed
    Disconnected { addr: SocketAddr },
    /// the port of the listener has changed
    PortChanged(Result<u16, ListenerCreationError>),
    /// emulation was disabled
    EmulationDisabled,
    /// emulation was enabled
    EmulationEnabled,
    /// capture should be released
    ReleaseNotify,
    /// The remote pointer was pushed past one of our screen edges, detected by
    /// the emulation backend that injects the peer's input. The peer must
    /// release its capture so the cursor can come back (return hand-off).
    EdgeReturn(lan_mouse_ipc::Position),
    /// The peer driving this machine released its capture (`Leave`). Its
    /// incoming capture zones can be dropped and, once no session is left, the
    /// local capture may resume.
    PeerLeft {
        addr: SocketAddr,
        fingerprint: String,
    },
    /// peer sent us a Hello with its build commit hash. Used to
    /// populate `client_manager.peer_commit` from the listen side
    /// too — without this, peer-version visibility silently fails
    /// whenever the outgoing connection in the *other* direction is
    /// broken (one-way setups, asymmetric NAT, peer's TCP listener
    /// down). The connect-side path stays as the primary source;
    /// this is the defensive fallback.
    PeerHello {
        addr: SocketAddr,
        commit: [u8; 8],
        proto_version: u8,
        capabilities: u32,
    },
    /// A peer announced the permissions it grants to this machine
    /// (incoming_caps) and expects to use here (outgoing_caps).
    PeerPermissions {
        addr: SocketAddr,
        fingerprint: String,
        incoming_caps: u8,
        outgoing_caps: u8,
    },
    /// A peer revoked the permission grants of its session.
    PeerAuthRevoked { addr: SocketAddr, reason: u8 },
}

/// Addresses of every live incoming session of \`fingerprint\`.
///
/// Used by revoke/deny-with-disconnect: a peer's session must be closed by its
/// certificate identity, not only when a capture zone already exists (peers
/// that are connected but not currently controlling this machine have no zone
/// in the service). Kept pure so the identity matching is unit-tested.
fn session_addrs_for_fingerprint(
    fingerprints: &HashMap<SocketAddr, String>,
    fingerprint: &str,
) -> Vec<SocketAddr> {
    let mut addrs: Vec<SocketAddr> = fingerprints
        .iter()
        .filter(|(_, fp)| fp.as_str() == fingerprint)
        .map(|(addr, _)| *addr)
        .collect();
    addrs.sort();
    addrs
}

enum EmulationRequest {
    Reenable,
    Release(SocketAddr, u32),
    /// close an active incoming connection (device revoked): also tells the
    /// peer this was an explicit user disconnect so it stops reconnecting
    Disconnect(SocketAddr),
    /// close an active incoming connection without the explicit-disconnect
    /// signal: the peer's own watchdog resets its capture. Used when a return
    /// hand-off cannot be confirmed (a lost datagram must not force the user
    /// to re-enable the device on the other machine).
    CloseSession(SocketAddr),
    /// close every live incoming session of a device identity (revoke/deny
    /// with disconnect), including sessions that never registered a capture
    /// zone
    DisconnectFingerprint(String),
    ChangePort(u16),
    Terminate,
}

/// Backends without edge-return support drop the notifier sender, closing the
/// channel. The emulation loop must not poll `recv()` once that happens: a
/// closed unbounded receiver returns `Ready(None)` immediately forever and
/// would spin a CPU core at 100% (Linux/macOS/Dummy backends).
fn edge_return_notifier_active(rx: &UnboundedReceiver<PointerEdge>) -> bool {
    !rx.is_closed()
}

impl Emulation {
    pub(crate) fn new(
        backend: Option<input_emulation::Backend>,
        listener: LanMouseListener,
        clipboard_tx: Sender<(std::net::SocketAddr, ClipboardFrame)>,
        devices: Option<Arc<RwLock<DeviceStore>>>,
    ) -> Self {
        let selected_backend: Rc<Cell<Option<input_emulation::Backend>>> = Default::default();
        let emulation_proxy = EmulationProxy::new(backend, selected_backend.clone());
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let (reply_tx, reply_rx) = channel();
        let emulation_task = ListenTask {
            listener,
            emulation_proxy,
            request_rx,
            event_tx,
            clipboard_tx,
            reply_rx,
            devices,
            fingerprints: Default::default(),
            #[cfg(not(target_os = "windows"))]
            edge_return: EdgeReturnTracker::default(),
        };
        let task = spawn_local(emulation_task.run());
        Self {
            task,
            request_tx,
            event_rx,
            selected_backend,
            reply_tx,
        }
    }

    /// Handle used by the clipboard task to reply on incoming connections.
    pub(crate) fn reply_handle(&self) -> Sender<ListenerCommand> {
        self.reply_tx.clone()
    }

    /// The emulation backend that was actually selected, once initialized.
    pub(crate) fn backend(&self) -> Option<input_emulation::Backend> {
        self.selected_backend.get()
    }

    /// Ask the peer at `addr` to release its capture (return hand-off).
    ///
    /// A non-zero `serial` marks the request as retried: the peer answers it
    /// again even if it already released, which is what lets the requester
    /// recover from a lost `Leave`.
    pub(crate) fn send_leave_request(&self, addr: SocketAddr, serial: u32) {
        self.request_tx
            .send(EmulationRequest::Release(addr, serial))
            .ok()
            .unwrap_or_default();
    }

    /// Close the incoming connection from `addr` (used when a device is
    /// revoked while its session is still live).
    pub(crate) fn disconnect(&self, addr: SocketAddr) {
        self.request_tx
            .send(EmulationRequest::Disconnect(addr))
            .ok()
            .unwrap_or_default();
    }

    /// Close the incoming connection from `addr` without the
    /// explicit-disconnect signal, so the peer's watchdog (not a "user
    /// disconnected" flag) resets its capture.
    pub(crate) fn close_session(&self, addr: SocketAddr) {
        self.request_tx
            .send(EmulationRequest::CloseSession(addr))
            .ok()
            .unwrap_or_default();
    }

    /// Close every live incoming session of \`fingerprint\` (revoke/deny).
    ///
    /// The peer is told this was an explicit user disconnect, so it deactivates
    /// its client instead of reconnecting.
    pub(crate) fn disconnect_fingerprint(&self, fingerprint: &str) {
        self.request_tx
            .send(EmulationRequest::DisconnectFingerprint(
                fingerprint.to_string(),
            ))
            .ok()
            .unwrap_or_default();
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(EmulationRequest::Reenable)
            .ok()
            .unwrap_or_default();
    }

    pub(crate) fn request_port_change(&self, port: u16) {
        self.request_tx
            .send(EmulationRequest::ChangePort(port))
            .ok()
            .unwrap_or_default()
    }

    pub(crate) async fn event(&mut self) -> EmulationEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    /// wait for termination
    pub(crate) async fn terminate(&mut self) {
        log::debug!("terminating emulation");
        self.request_tx
            .send(EmulationRequest::Terminate)
            .ok()
            .unwrap_or_default();
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }
}

/// Deduplicates `Enter` retransmissions per connection.
///
/// Serial `0` means a peer predating the serial field; those events are
/// never deduplicated because legacy peers always send `0`.
#[derive(Debug, Default)]
struct EnterDeduper {
    last: HashMap<SocketAddr, u32>,
}

impl EnterDeduper {
    /// Whether this `Enter` is a retransmission of the last handled one.
    fn is_duplicate(&self, addr: SocketAddr, serial: u32) -> bool {
        serial != 0 && self.last.get(&addr) == Some(&serial)
    }

    /// Remember a successfully handled `Enter`.
    fn record(&mut self, addr: SocketAddr, serial: u32) {
        self.last.insert(addr, serial);
    }

    fn clear(&mut self, addr: SocketAddr) {
        self.last.remove(&addr);
    }
}

struct ListenTask {
    listener: LanMouseListener,
    emulation_proxy: EmulationProxy,
    request_rx: Receiver<EmulationRequest>,
    event_tx: Sender<EmulationEvent>,
    clipboard_tx: Sender<(std::net::SocketAddr, ClipboardFrame)>,
    reply_rx: Receiver<ListenerCommand>,
    /// device policies used to answer permission negotiations
    devices: Option<Arc<RwLock<DeviceStore>>>,
    /// peer fingerprint per live connection, used for permission events
    fingerprints: HashMap<SocketAddr, String>,
    /// software return hand-off detection for emulation backends that cannot
    /// report screen edges (everything but Windows; see crate::edge_return)
    #[cfg(not(target_os = "windows"))]
    edge_return: EdgeReturnTracker,
}

impl ListenTask {
    /// Tell the peer that this is a deliberate user disconnect.
    ///
    /// Events travel over DTLS/UDP without retransmission. A single lost
    /// AuthRevoke made the peer treat the close as a network drop and reconnect
    /// (the Disconnect button appeared to blink back), so send a few copies
    /// with short spacing before closing the transport.
    async fn explicit_disconnect(&mut self, addr: SocketAddr) {
        for copy in 0..5 {
            self.listener
                .reply(
                    addr,
                    ProtoEvent::AuthRevoke {
                        session_id: 0,
                        reason: 1,
                    },
                )
                .await;
            if copy + 1 < 5 {
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        }
        self.listener.disconnect(addr).await;
    }

    async fn run(mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let mut last_response = HashMap::new();
        // Last `Enter` serial seen per connection, used to swallow
        // retransmissions without re-triggering `ReleaseNotify`.
        let mut last_enter = EnterDeduper::default();
        let mut rejected_connections = HashMap::new();
        loop {
            select! {
                e = self.listener.next() => {match e {
                    Some(ListenEvent::Msg { event, addr }) => {
                        log::trace!("{event} <-<-<-<-<- {addr}");
                        last_response.insert(addr, Instant::now());
                        match event {
                            ProtoEvent::Enter { pos, serial } => {
                                let duplicate = last_enter.is_duplicate(addr, serial);
                                if duplicate {
                                    log::debug!("duplicate Enter({serial}) from {addr}, re-acking");
                                    self.listener.reply(addr, ProtoEvent::Ack(serial)).await;
                                } else if let Some(fingerprint) = self.listener.get_certificate_fingerprint(addr).await {
                                    last_enter.record(addr, serial);
                                    #[cfg(not(target_os = "windows"))]
                                    self.edge_return.enter(addr, to_ipc_pos(pos));
                                    log::info!("releasing capture: {addr} entered this device");
                                    // Publish the hand-off before the Ack gives the
                                    // peer permission to start injecting input. The
                                    // service must have installed the remote-peer
                                    // suppression by then; otherwise the first
                                    // injected motion can be mistaken for local
                                    // input and both machines ping-pong control at
                                    // the shared edge (the old enter/leave cycle).
                                    self.event_tx.send(EmulationEvent::ReleaseNotify).ok().unwrap_or_default();
                                    self.event_tx.send(EmulationEvent::Entered{addr, pos: to_ipc_pos(pos), fingerprint}).ok().unwrap_or_default();
                                    self.listener.reply(addr, ProtoEvent::Ack(serial)).await;
                                }
                            }
                            ProtoEvent::Leave(_) => {
                                #[cfg(not(target_os = "windows"))]
                                self.edge_return.leave(addr);
                                self.emulation_proxy.remove(addr);
                                self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                                // The peer is no longer driving this machine.
                                // Without this the service kept the incoming
                                // capture zone (and on Windows the remote-active
                                // suppression) latched until the socket dropped.
                                let fingerprint = self
                                    .fingerprints
                                    .get(&addr)
                                    .cloned()
                                    .unwrap_or_default();
                                self.event_tx
                                    .send(EmulationEvent::PeerLeft { addr, fingerprint })
                                    .ok()
                                    .unwrap_or_default();
                            }
                            ProtoEvent::Input(event) => {
                                // On backends without native edge reporting the
                                // injected motion itself is the only evidence
                                // that the user pushed the pointer back out
                                // through the entry edge.
                                #[cfg(not(target_os = "windows"))]
                                if let Some(pos) = self.edge_return.observe(addr, &event) {
                                    log::info!(
                                        "{addr} pushed the pointer back out through {pos:?}; requesting capture release"
                                    );
                                    self.event_tx
                                        .send(EmulationEvent::EdgeReturn(pos))
                                        .ok()
                                        .unwrap_or_default();
                                }
                                self.emulation_proxy.consume(event, addr)
                            }
                            ProtoEvent::AuthRequest {
                                session_id,
                                incoming_caps,
                                outgoing_caps,
                            } => {
                                let fingerprint = self
                                    .fingerprints
                                    .get(&addr)
                                    .cloned()
                                    .unwrap_or_default();
                                let (incoming, outgoing) = match (&self.devices, fingerprint.as_str()) {
                                    (Some(devices), fingerprint) if !fingerprint.is_empty() => {
                                        let store = devices.read().expect("lock");
                                        (store.incoming(fingerprint), store.outgoing(fingerprint))
                                    }
                                    _ => (
                                        lan_mouse_ipc::DirectionPolicy::allow_all(),
                                        lan_mouse_ipc::DirectionPolicy::allow_all(),
                                    ),
                                };
                                self.listener
                                    .reply(
                                        addr,
                                        ProtoEvent::AuthResponse {
                                            session_id,
                                            incoming_caps: incoming.capabilities.0,
                                            outgoing_caps: outgoing.capabilities.0,
                                        },
                                    )
                                    .await;
                                self.event_tx
                                    .send(EmulationEvent::PeerPermissions {
                                        addr,
                                        fingerprint,
                                        incoming_caps,
                                        outgoing_caps,
                                    })
                                    .ok()
                                    .unwrap_or_default();
                            }
                            ProtoEvent::AuthRevoke { reason, .. } => {
                                self.event_tx
                                    .send(EmulationEvent::PeerAuthRevoked { addr, reason })
                                    .ok()
                                    .unwrap_or_default();
                            }
                            ProtoEvent::Ping => self.listener.reply(addr, ProtoEvent::Pong {
                                alive: self.emulation_proxy.emulation_active.get(),
                                // Advertise what we understand so the peer
                                // can enable the extensions safely. Legacy
                                // peers ignore the trailing bytes.
                                capabilities: Some(CAPABILITIES),
                            }).await,
                            // Peer's version handshake. Echo our own
                            // commit back so the peer's connect-side
                            // receive_loop populates its `peer_commit`,
                            // AND publish a PeerHello upward so our
                            // service can populate ours from the listen
                            // side too — the connect side is the primary
                            // path, but if the outbound direction is
                            // broken (one-way setup, NAT, peer's TCP
                            // listener down) the version display would
                            // otherwise silently say "unknown" while
                            // the peer is in fact happily talking to us.
                            ProtoEvent::Hello { commit, proto_version, capabilities } => {
                                self.listener.reply(addr, ProtoEvent::Hello {
                                    commit: local_commit(),
                                    proto_version: PROTO_VERSION,
                                    capabilities: CAPABILITIES,
                                }).await;
                                self.event_tx.send(EmulationEvent::PeerHello {
                                    addr,
                                    commit,
                                    proto_version,
                                    capabilities,
                                }).ok().unwrap_or_default();
                            }
                            _ => {}
                        }
                    }
                    Some(ListenEvent::Clipboard { frame, addr }) => {
                        if self.clipboard_tx.send((addr, frame)).is_err() {
                            log::debug!(
                                "clipboard task is not running, dropping a frame from {addr}"
                            );
                        }
                    }
                    Some(ListenEvent::Accept { addr, fingerprint }) => {
                        // A fresh connection from this address starts a new
                        // serial space; never dedupe against the old one.
                        self.fingerprints.insert(addr, fingerprint.clone());
                        last_enter.clear(addr);
                        last_response.insert(addr, Instant::now());
                        self.event_tx.send(EmulationEvent::Connected { addr, fingerprint }).ok().unwrap_or_default();
                    }
                    Some(ListenEvent::AutoAuthorized {
                        addr,
                        fingerprint,
                        from_pairing,
                        incoming,
                        outgoing,
                    }) => {
                        self.event_tx
                            .send(EmulationEvent::AutoAuthorized {
                                addr,
                                fingerprint,
                                from_pairing,
                                incoming,
                                outgoing,
                            })
                            .ok()
                            .unwrap_or_default();
                    }
                    Some(ListenEvent::Closed { addr }) => {
                        self.fingerprints.remove(&addr);
                        if last_response.remove(&addr).is_some() {
                            self.emulation_proxy.remove(addr);
                            last_enter.clear(addr);
                            self.event_tx
                                .send(EmulationEvent::Disconnected { addr })
                                .ok()
                                .unwrap_or_default();
                        }
                    }
                    Some(ListenEvent::Rejected { addr, fingerprint }) => {
                        if rejected_connections.insert(fingerprint.clone(), Instant::now())
                            .is_none_or(|i| i.elapsed() >= Duration::from_secs(2)) {
                                self.event_tx.send(EmulationEvent::ConnectionAttempt { addr, fingerprint }).ok().unwrap_or_default();
                            }
                    }
                    None => break
                }}
                event = self.emulation_proxy.event() => {
                    self.event_tx.send(event).ok().unwrap_or_default();
                }
                command = self.reply_rx.recv() => match command.expect("channel closed") {
                    ListenerCommand::Reply(addr, bytes) => self.listener.reply_raw(addr, &bytes).await,
                },
                request = self.request_rx.recv() => match request.expect("channel closed") {
                    // reenable emulation
                    EmulationRequest::Reenable => self.emulation_proxy.reenable(),
                    // notify the other end that we hit a barrier (should release capture)
                    EmulationRequest::Release(addr, serial) => {
                        self.listener.reply(addr, ProtoEvent::Leave(serial)).await
                    }
                    EmulationRequest::Disconnect(addr) => {
                        // Tell the peer this is an explicit user disconnect so
                        // it deactivates its client instead of reconnecting.
                        self.explicit_disconnect(addr).await;
                        #[cfg(not(target_os = "windows"))]
                        self.edge_return.leave(addr);
                    }
                    EmulationRequest::CloseSession(addr) => {
                        // No AuthRevoke: a forced "user disconnect" would make
                        // the peer deactivate the device until it is manually
                        // re-enabled - far worse than the recovery this is for.
                        self.listener.disconnect(addr).await
                    }
                    EmulationRequest::DisconnectFingerprint(fingerprint) => {
                        // Close every session of this identity, not only the
                        // ones that already registered a capture zone: a revoke
                        // of a connected peer used to leave its DTLS session
                        // alive (peer stayed active with the cursor captured).
                        let addrs =
                            session_addrs_for_fingerprint(&self.fingerprints, &fingerprint);
                        for addr in addrs {
                            log::info!(
                                "closing incoming session from {addr}: device {fingerprint} revoked"
                            );
                            self.explicit_disconnect(addr).await;
                            #[cfg(not(target_os = "windows"))]
                            self.edge_return.leave(addr);
                        }
                        // Also ask the listener to close sessions known by
                        // identity but not yet in the emulation map.
                        self.listener.disconnect_fingerprint(&fingerprint).await;
                    }
                    EmulationRequest::ChangePort(port) => {
                        self.listener.request_port_change(port);
                        let result = self.listener.port_changed().await;
                        self.event_tx.send(EmulationEvent::PortChanged(result)).ok().unwrap_or_default();
                    }
                    EmulationRequest::Terminate => break,
                },
                _ = interval.tick() => {
                    // A peer that stops sending pings/input is gone: release
                    // its held keys AND close the incoming transport. Before
                    // this, the DTLS session was kept forever, so dirty
                    // disconnects accumulated in the listener until the
                    // connection cap rejected every new peer until restart.
                    let now = Instant::now();
                    let expired: Vec<SocketAddr> = last_response
                        .iter()
                        .filter(|(_, seen)| now.duration_since(**seen) > EMULATION_TIMEOUT)
                        .map(|(addr, _)| *addr)
                        .collect();
                    for addr in expired {
                        log::warn!("releasing keys: {addr} not responding!");
                        self.emulation_proxy.remove(addr);
                        last_response.remove(&addr);
                        last_enter.clear(addr);
                        #[cfg(not(target_os = "windows"))]
                        self.edge_return.leave(addr);
                        self.listener.disconnect(addr).await;
                        self.event_tx.send(EmulationEvent::Disconnected { addr }).ok().unwrap_or_default();
                    }
                    // Rejected handshakes are deduplicated for a short window;
                    // drop old entries so a stream of random certificates cannot
                    // grow the map forever.
                    rejected_connections
                        .retain(|_, seen| seen.elapsed() < Duration::from_secs(60));
                }
            }
        }
        self.listener.terminate().await;
        self.emulation_proxy.terminate().await;
    }
}

/// proxy handling the actual input emulation,
/// discarding events when it is disabled
pub(crate) struct EmulationProxy {
    emulation_active: Rc<Cell<bool>>,
    exit_requested: Rc<Cell<bool>>,
    request_tx: Sender<ProxyRequest>,
    event_rx: Receiver<EmulationEvent>,
    task: JoinHandle<()>,
}

enum ProxyRequest {
    Input(Event, SocketAddr),
    Remove(SocketAddr),
    Terminate,
    Reenable,
}

impl EmulationProxy {
    fn new(
        backend: Option<input_emulation::Backend>,
        selected_backend: Rc<Cell<Option<input_emulation::Backend>>>,
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_active = Rc::new(Cell::new(false));
        let exit_requested = Rc::new(Cell::new(false));
        let emulation_task = EmulationTask {
            backend,
            exit_requested: exit_requested.clone(),
            request_rx,
            event_tx,
            handles: Default::default(),
            next_id: 0,
            selected_backend,
        };
        let task = spawn_local(emulation_task.run());
        Self {
            emulation_active,
            exit_requested,
            request_tx,
            task,
            event_rx,
        }
    }

    async fn event(&mut self) -> EmulationEvent {
        let event = self.event_rx.recv().await.expect("channel closed");
        if let EmulationEvent::EmulationEnabled = event {
            self.emulation_active.replace(true);
        }
        if let EmulationEvent::EmulationDisabled = event {
            self.emulation_active.replace(false);
        }
        event
    }

    fn consume(&self, event: Event, addr: SocketAddr) {
        // ignore events if emulation is currently disabled
        if self.emulation_active.get() {
            self.request_tx
                .send(ProxyRequest::Input(event, addr))
                .ok()
                .unwrap_or_default();
        }
    }

    fn remove(&self, addr: SocketAddr) {
        self.request_tx
            .send(ProxyRequest::Remove(addr))
            .ok()
            .unwrap_or_default();
    }

    fn reenable(&self) {
        self.request_tx
            .send(ProxyRequest::Reenable)
            .ok()
            .unwrap_or_default();
    }

    async fn terminate(&mut self) {
        self.exit_requested.replace(true);
        self.request_tx
            .send(ProxyRequest::Terminate)
            .ok()
            .unwrap_or_default();
        let _ = (&mut self.task).await;
    }
}

struct EmulationTask {
    backend: Option<input_emulation::Backend>,
    exit_requested: Rc<Cell<bool>>,
    request_rx: Receiver<ProxyRequest>,
    event_tx: Sender<EmulationEvent>,
    handles: HashMap<SocketAddr, EmulationHandle>,
    next_id: EmulationHandle,
    selected_backend: Rc<Cell<Option<input_emulation::Backend>>>,
}

impl EmulationTask {
    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_emulation().await {
                log::warn!("input emulation exited: {e}");
            }
            if self.exit_requested.get() {
                break;
            }
            // wait for reenable request
            loop {
                match self.request_rx.recv().await.expect("channel closed") {
                    ProxyRequest::Reenable => break,
                    ProxyRequest::Terminate => return,
                    ProxyRequest::Input(..) => { /* emulation inactive => ignore */ }
                    ProxyRequest::Remove(..) => { /* emulation inactive => ignore */ }
                }
            }
        }
    }

    async fn do_emulation(&mut self) -> Result<(), InputEmulationError> {
        log::info!("creating input emulation ...");
        let mut emulation = tokio::select! {
            r = InputEmulation::new(self.backend) => r?,
            // allow termination event while requesting input emulation
            _ = wait_for_termination(&mut self.request_rx) => return Ok(()),
        };
        self.selected_backend.set(Some(emulation.backend()));

        // The emulation backend that injects the peer's input is the component
        // that can see the remote pointer being pushed past a screen edge on
        // platforms where the capture backend cannot (absolute Windows
        // injection, injected motion that does not trigger compositor
        // barriers). It reports the hand-off request through this channel.
        let (edge_return_tx, mut edge_return_rx) = tokio::sync::mpsc::unbounded_channel();
        emulation.set_edge_return_notifier(edge_return_tx);

        // used to send enabled and disabled events
        let _emulation_guard = DropGuard::new(
            self.event_tx.clone(),
            EmulationEvent::EmulationEnabled,
            EmulationEvent::EmulationDisabled,
        );

        // create active handles
        if let Err(e) = self.create_clients(&mut emulation).await {
            emulation.terminate().await;
            return Err(e);
        }

        let res = self
            .do_emulation_session(&mut emulation, &mut edge_return_rx)
            .await;
        // FIXME replace with async drop when stabilized
        emulation.terminate().await;
        res
    }

    async fn create_clients(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        for handle in self.handles.values() {
            tokio::select! {
                _ = emulation.create(*handle) => {},
                _ = wait_for_termination(&mut self.request_rx) => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_emulation_session(
        &mut self,
        emulation: &mut InputEmulation,
        edge_return_rx: &mut UnboundedReceiver<PointerEdge>,
    ) -> Result<(), InputEmulationError> {
        loop {
            tokio::select! {
                // Backends without edge-return support drop the notifier in
                // the default `set_edge_return_notifier` implementation, so
                // the channel is already closed for them. Without this guard
                // `recv()` would return `Ready(None)` forever and spin a CPU
                // core at 100% (Linux/macOS/Dummy backends).
                edge = edge_return_rx.recv(), if edge_return_notifier_active(edge_return_rx) => {
                    if let Some(edge) = edge {
                        log::debug!("emulation observed a return hand-off through {edge:?}");
                        self.event_tx
                            .send(EmulationEvent::EdgeReturn(edge_to_ipc_pos(edge)))
                            .ok()
                            .unwrap_or_default();
                    } else {
                        log::debug!("edge-return notifier closed; hand-off disabled");
                    }
                },
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    ProxyRequest::Input(event, addr) => {
                        let handle = match self.handles.get(&addr) {
                            Some(&handle) => handle,
                            None => {
                                let handle = self.next_id;
                                self.next_id += 1;
                                emulation.create(handle).await;
                                self.handles.insert(addr, handle);
                                handle
                            }
                        };
                        emulation.consume(event, handle).await?;
                    },
                    ProxyRequest::Remove(addr) => {
                        if let Some(handle) = self.handles.remove(&addr) {
                            emulation.destroy(handle).await;
                        }
                    }
                    ProxyRequest::Terminate => break Ok(()),
                    ProxyRequest::Reenable => continue,
                },
            }
        }
    }
}

fn edge_to_ipc_pos(edge: PointerEdge) -> lan_mouse_ipc::Position {
    match edge {
        PointerEdge::Left => lan_mouse_ipc::Position::Left,
        PointerEdge::Right => lan_mouse_ipc::Position::Right,
        PointerEdge::Top => lan_mouse_ipc::Position::Top,
        PointerEdge::Bottom => lan_mouse_ipc::Position::Bottom,
    }
}

fn to_ipc_pos(pos: Position) -> lan_mouse_ipc::Position {
    match pos {
        Position::Left => lan_mouse_ipc::Position::Left,
        Position::Right => lan_mouse_ipc::Position::Right,
        Position::Top => lan_mouse_ipc::Position::Top,
        Position::Bottom => lan_mouse_ipc::Position::Bottom,
    }
}

async fn wait_for_termination(rx: &mut Receiver<ProxyRequest>) {
    loop {
        match rx.recv().await.expect("channel closed") {
            ProxyRequest::Terminate => return,
            ProxyRequest::Input(_, _) => continue,
            ProxyRequest::Remove(_) => continue,
            ProxyRequest::Reenable => continue,
        }
    }
}

struct DropGuard<T> {
    tx: Sender<T>,
    on_drop: Option<T>,
}

impl<T> DropGuard<T> {
    fn new(tx: Sender<T>, on_new: T, on_drop: T) -> Self {
        tx.send(on_new).ok().unwrap_or_default();
        let on_drop = Some(on_drop);
        Self { tx, on_drop }
    }
}

impl<T> Drop for DropGuard<T> {
    fn drop(&mut self) {
        self.tx
            .send(self.on_drop.take().expect("item"))
            .ok()
            .unwrap_or_default();
    }
}

#[cfg(test)]
mod enter_deduper_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn addr(last: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, last)), 4242)
    }

    #[test]
    fn repeated_serial_is_a_duplicate() {
        let mut deduper = EnterDeduper::default();
        assert!(!deduper.is_duplicate(addr(1), 5));
        deduper.record(addr(1), 5);
        assert!(deduper.is_duplicate(addr(1), 5));
    }

    #[test]
    fn new_serial_is_not_a_duplicate() {
        let mut deduper = EnterDeduper::default();
        assert!(!deduper.is_duplicate(addr(1), 5));
        deduper.record(addr(1), 5);
        assert!(!deduper.is_duplicate(addr(1), 6));
        deduper.record(addr(1), 6);
        assert!(
            !deduper.is_duplicate(addr(1), 5),
            "an older serial is not a retry"
        );
    }

    #[test]
    fn legacy_serial_zero_is_never_deduplicated() {
        let mut deduper = EnterDeduper::default();
        assert!(!deduper.is_duplicate(addr(1), 0));
        deduper.record(addr(1), 0);
        assert!(!deduper.is_duplicate(addr(1), 0));
    }

    #[test]
    fn connections_are_isolated() {
        let mut deduper = EnterDeduper::default();
        deduper.record(addr(1), 5);
        assert!(!deduper.is_duplicate(addr(2), 5), "different peer");
    }

    #[test]
    fn clear_forgets_the_serial_space() {
        let mut deduper = EnterDeduper::default();
        deduper.record(addr(1), 5);
        assert!(deduper.is_duplicate(addr(1), 5));
        deduper.clear(addr(1));
        assert!(!deduper.is_duplicate(addr(1), 5));
    }
}

#[cfg(test)]
mod edge_return_guard_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn closed_edge_return_channel_is_disabled() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<PointerEdge>();
        assert!(edge_return_notifier_active(&rx));
        drop(tx);
        assert!(!edge_return_notifier_active(&rx));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn closed_edge_return_arm_does_not_busy_loop() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PointerEdge>();
        drop(tx);
        let mut arm_polls = 0usize;
        let result = tokio::time::timeout(Duration::from_millis(100), async {
            loop {
                tokio::select! {
                    edge = rx.recv(), if edge_return_notifier_active(&rx) => {
                        arm_polls += 1;
                        let _ = edge;
                    }
                    _ = tokio::task::yield_now() => break,
                }
            }
        })
        .await;
        assert!(result.is_ok(), "the loop should yield, not spin");
        assert_eq!(arm_polls, 0, "a closed notifier must be disabled");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_addrs_match_by_full_fingerprint_identity() {
        let mut map = HashMap::new();
        let a: SocketAddr = "127.0.0.1:1000".parse().expect("addr");
        let b: SocketAddr = "127.0.0.2:2000".parse().expect("addr");
        let c: SocketAddr = "127.0.0.3:3000".parse().expect("addr");
        map.insert(a, "aa:bb".to_string());
        map.insert(b, "aa:bb".to_string());
        map.insert(c, "cc:dd".to_string());

        assert_eq!(
            session_addrs_for_fingerprint(&map, "aa:bb"),
            vec![a, b],
            "every live session of the identity must be returned"
        );
        assert_eq!(session_addrs_for_fingerprint(&map, "cc:dd"), vec![c]);
        assert!(
            session_addrs_for_fingerprint(&map, "aa").is_empty(),
            "a fingerprint prefix must not match"
        );
        assert!(session_addrs_for_fingerprint(&map, "aa:bb:cc").is_empty());
        assert!(session_addrs_for_fingerprint(&map, "").is_empty());
        assert!(session_addrs_for_fingerprint(&HashMap::new(), "aa:bb").is_empty());
    }
}
