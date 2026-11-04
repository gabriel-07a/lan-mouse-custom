use crate::config::local_commit;
use crate::listen::{LanMouseListener, ListenEvent, ListenerCommand, ListenerCreationError};
use futures::StreamExt;
use input_emulation::{EmulationHandle, InputEmulation, InputEmulationError};
use input_event::Event;
use lan_mouse_proto::clipboard::ClipboardFrame;
use lan_mouse_proto::{CAPABILITIES, PROTO_VERSION, Position, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::Cell,
    collections::HashMap,
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::{
    select,
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
    ConnectionAttempt {
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
    Disconnected {
        addr: SocketAddr,
    },
    /// the port of the listener has changed
    PortChanged(Result<u16, ListenerCreationError>),
    /// emulation was disabled
    EmulationDisabled,
    /// emulation was enabled
    EmulationEnabled,
    /// capture should be released
    ReleaseNotify,
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
}

enum EmulationRequest {
    Reenable,
    Release(SocketAddr),
    ChangePort(u16),
    Terminate,
}

impl Emulation {
    pub(crate) fn new(
        backend: Option<input_emulation::Backend>,
        listener: LanMouseListener,
        clipboard_tx: Sender<(std::net::SocketAddr, ClipboardFrame)>,
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

    pub(crate) fn send_leave_event(&self, addr: SocketAddr) {
        self.request_tx
            .send(EmulationRequest::Release(addr))
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
}

impl ListenTask {
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
                                    log::info!("releasing capture: {addr} entered this device");
                                    self.event_tx.send(EmulationEvent::ReleaseNotify).ok().unwrap_or_default();
                                    self.listener.reply(addr, ProtoEvent::Ack(serial)).await;
                                    self.event_tx.send(EmulationEvent::Entered{addr, pos: to_ipc_pos(pos), fingerprint}).ok().unwrap_or_default();
                                }
                            }
                            ProtoEvent::Leave(_) => {
                                self.emulation_proxy.remove(addr);
                                self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                            }
                            ProtoEvent::Input(event) => self.emulation_proxy.consume(event, addr),
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
                        last_enter.clear(addr);
                        self.event_tx.send(EmulationEvent::Connected { addr, fingerprint }).ok().unwrap_or_default();
                    }
                    Some(ListenEvent::Rejected { fingerprint }) => {
                        if rejected_connections.insert(fingerprint.clone(), Instant::now())
                            .is_none_or(|i| i.elapsed() >= Duration::from_secs(2)) {
                                self.event_tx.send(EmulationEvent::ConnectionAttempt { fingerprint }).ok().unwrap_or_default();
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
                    EmulationRequest::Release(addr) => self.listener.reply(addr, ProtoEvent::Leave(0)).await,
                    EmulationRequest::ChangePort(port) => {
                        self.listener.request_port_change(port);
                        let result = self.listener.port_changed().await;
                        self.event_tx.send(EmulationEvent::PortChanged(result)).ok().unwrap_or_default();
                    }
                    EmulationRequest::Terminate => break,
                },
                _ = interval.tick() => {
                    last_response.retain(|&addr,instant| {
                        if instant.elapsed() > EMULATION_TIMEOUT {
                            log::warn!("releasing keys: {addr} not responding!");
                            self.emulation_proxy.remove(addr);
                            self.event_tx.send(EmulationEvent::Disconnected { addr }).ok().unwrap_or_default();
                            last_enter.clear(addr);
                            false
                        } else {
                            true
                        }
                    });
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

        let res = self.do_emulation_session(&mut emulation).await;
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
    ) -> Result<(), InputEmulationError> {
        loop {
            tokio::select! {
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
