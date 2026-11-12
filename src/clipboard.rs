//! Clipboard sync task.
//!
//! Bridges the platform-independent [`input_clipboard`] state machine to the
//! network: outgoing frames go through the same DTLS connection used for
//! input, incoming frames arrive from the listener (peer dialed us) or from
//! the outgoing connection (we dialed the peer).

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    rc::Rc,
    sync::{Arc, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures::FutureExt;
use input_clipboard::{
    Clipboard, ClipboardSnapshot, Limits,
    sync::{ClipboardSync, FrameSink, PeerId},
};
use lan_mouse_ipc::{ClientHandle, ClipboardStatus};
use lan_mouse_proto::capabilities::CLIPBOARD;
use lan_mouse_proto::clipboard::ClipboardFrame;
use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::task::{JoinHandle, spawn_local};

use crate::{
    client::ClientManager, connect::ConnectionSender, devices::DeviceStore, listen::ListenerCommand,
};

/// How a peer is reachable.
#[derive(Clone, Copy, Debug)]
enum Route {
    /// Peer configured as a client: send over our outgoing DTLS connection.
    Outgoing(ClientHandle),
    /// Peer dialed us: reply on the listener's connection.
    Incoming(SocketAddr),
}

#[derive(Clone)]
struct Sink {
    routes: Rc<RefCell<HashMap<PeerId, Route>>>,
    tx: Sender<(Route, Vec<u8>)>,
    client_manager: ClientManager,
    devices: Arc<RwLock<DeviceStore>>,
}

impl Sink {
    /// Whether this peer may read our clipboard.
    ///
    /// The outgoing direction is gated by the peer's `incoming` policy: a
    /// device granted only `clipboard-write` may set our clipboard but must
    /// not receive it. Unknown fingerprints (legacy/unpinned links) keep the
    /// permissive behavior.
    fn peer_may_read(&self, route: Route) -> bool {
        let fingerprint = match route {
            Route::Outgoing(handle) => self.client_manager.fingerprint(handle),
            Route::Incoming(addr) => self
                .client_manager
                .find_by_ip(addr.ip())
                .and_then(|handle| self.client_manager.fingerprint(handle)),
        };
        let Some(fingerprint) = fingerprint else {
            return true;
        };
        let store = self.devices.read().expect("lock");
        let policy = store.incoming(&fingerprint);
        policy.allowed && policy.capabilities.allows_clipboard_read()
    }
}

impl FrameSink for Sink {
    fn send(&self, peer: PeerId, frame: ClipboardFrame) {
        let route = self.routes.borrow().get(&peer).copied();
        if let Some(route) = route {
            // Only clipboard *content* is gated by the peer's clipboard-read
            // permission. Acks and cancels are control replies for a transfer
            // the peer already sent us; dropping them (as the old blanket
            // check did) made a device with clipboard-write-only permission
            // impossible to transfer to us in the opposite direction.
            if frame.is_content() && !self.peer_may_read(route) {
                log::debug!("clipboard: peer may not read our clipboard, dropping frame");
                return;
            }
            let _ = self.tx.send((route, frame.encode()));
        }
    }
}

/// A remote snapshot whose write to the local clipboard failed.
///
/// The sender's transfer is kept alive (its final acknowledgement is withheld)
/// while the write is retried with a bounded backoff, so a transient backend
/// failure (portal hiccup, clipboard held open) does not silently lose the
/// copy. A permanently failing backend abandons the snapshot and tells the
/// sender to re-offer later.
struct PendingApply {
    snapshot: ClipboardSnapshot,
    attempts: u32,
    next_at: Instant,
}

impl PendingApply {
    /// Give up after this many failed writes (~4.7 s with the backoff below).
    const MAX_ATTEMPTS: u32 = 7;

    fn new(snapshot: ClipboardSnapshot) -> Self {
        Self {
            snapshot,
            attempts: 0,
            next_at: Instant::now(),
        }
    }

    fn due(&self, now: Instant) -> bool {
        now >= self.next_at
    }

    /// Record a failed write; returns whether it should be retried.
    fn on_failure(&mut self, now: Instant) -> bool {
        self.attempts += 1;
        if self.attempts >= Self::MAX_ATTEMPTS {
            return false;
        }
        let factor = match self.attempts {
            0 | 1 => 1,
            2 => 2,
            3 => 4,
            4 => 8,
            _ => 16,
        };
        self.next_at = now + APPLY_RETRY_BASE * factor;
        true
    }
}

/// A local copy supersedes a remote snapshot that is still waiting to be
/// written: applying it later would overwrite the user's newer copy, so the
/// pending write is abandoned and its sender is told to re-offer.
fn cancel_pending_apply(pending: &mut Option<PendingApply>, sync: &mut ClipboardSync<Sink>) {
    if let Some(apply) = pending.take() {
        sync.on_apply_failed(&apply.snapshot);
    }
}

/// Base delay for retrying a failed clipboard write.
const APPLY_RETRY_BASE: Duration = Duration::from_millis(100);
/// Retry delay for reading the current clipboard when a peer registers.
const PUSH_RETRY_DELAY: Duration = Duration::from_millis(250);

/// Remove and return the address-registered peers of `ip`.
///
/// Incoming sessions are keyed by socket address, outgoing clients by handle.
/// Frames from a peer can arrive before its `Hello` marks the outgoing client
/// as connected, creating a second identity for the same peer. Merging by IP
/// (the fork is IPv4-only, and clients are matched by IP elsewhere too) keeps
/// one identity instead of tearing down in-flight transfers.
fn take_address_peers_for_ip(
    addr_peers: &mut HashMap<SocketAddr, PeerId>,
    ip: IpAddr,
) -> Vec<(SocketAddr, PeerId)> {
    let stale: Vec<SocketAddr> = addr_peers
        .keys()
        .filter(|addr| addr.ip() == ip)
        .copied()
        .collect();
    stale
        .into_iter()
        .filter_map(|addr| addr_peers.remove(&addr).map(|peer| (addr, peer)))
        .collect()
}

pub(crate) struct ClipboardTask {
    task: JoinHandle<()>,
}

/// Why the clipboard sync loop returned.
enum RunExit {
    /// The service (or the listener) is shutting down.
    Shutdown,
    /// The platform backend's change stream ended. The backend can be
    /// recreated, e.g. after a portal session restart.
    BackendStopped,
}

/// The channels fed by the listener and the connection tasks. Passed by
/// reference so the backend can be recreated without losing queued frames.
struct Channels {
    from_listener: Receiver<(SocketAddr, ClipboardFrame)>,
    from_connection: Receiver<(ClientHandle, ClipboardFrame)>,
    peer_events: Receiver<SocketAddr>,
    /// Capabilities advertised by peers that dialed us (from their `Hello`).
    /// Shared with the service for the return-hand-off extension and used here
    /// to register incoming clipboard peers.
    incoming_caps: Arc<RwLock<HashMap<SocketAddr, u32>>>,
}

/// Wait until the user enables clipboard sync, dropping queued frames while
/// waiting so a peer cannot grow the unbounded channels.
async fn wait_for_enable(channels: &mut Channels, enabled: &Rc<Cell<bool>>) {
    while !enabled.get() {
        drain_frames(&mut channels.from_listener);
        drain_frames(&mut channels.from_connection);
        drain_frames(&mut channels.peer_events);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait until the user disables clipboard sync (used after a backend failed,
/// so a denied permission dialog is not re-opened in a loop).
async fn wait_until_disabled(channels: &mut Channels, enabled: &Rc<Cell<bool>>) {
    while enabled.get() {
        drain_frames(&mut channels.from_listener);
        drain_frames(&mut channels.from_connection);
        drain_frames(&mut channels.peer_events);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Create the platform clipboard backend.
///
/// A backend that died is retried automatically (`retries > 0`); an initial
/// failure (for example a denied portal consent) waits for the user to toggle
/// clipboard sync off and on again instead of re-prompting in a loop.
async fn create_backend(
    channels: &mut Channels,
    enabled: &Rc<Cell<bool>>,
    status: &mut ClipboardStatus,
    status_tx: &Sender<ClipboardStatus>,
    retries: u32,
) -> Box<dyn Clipboard> {
    let mut retries = retries;
    loop {
        if !enabled.get() {
            wait_for_enable(channels, enabled).await;
        }
        match input_clipboard::default_backend().await {
            Ok(backend) => return backend,
            Err(e) => {
                status.enabled = enabled.get();
                status.available = false;
                status.last_error = Some(e.to_string());
                log::info!("clipboard backend unavailable: {e}");
                let _ = status_tx.send(status.clone());
                if retries > 0 {
                    retries -= 1;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                } else {
                    wait_until_disabled(channels, enabled).await;
                }
            }
        }
    }
}

impl ClipboardTask {
    /// Spawn the clipboard task.
    ///
    /// Backend creation can block on a portal permission dialog, so it runs
    /// in the background: the service starts immediately, and clipboard sync
    /// becomes active once the backend is ready. The task also waits for the
    /// user to enable clipboard sync if it starts disabled.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        client_manager: ClientManager,
        sender: ConnectionSender,
        replies: Sender<ListenerCommand>,
        from_listener: Receiver<(SocketAddr, ClipboardFrame)>,
        from_connection: Receiver<(ClientHandle, ClipboardFrame)>,
        peer_events: Receiver<SocketAddr>,
        incoming_caps: Arc<RwLock<HashMap<SocketAddr, u32>>>,
        limits: Rc<Cell<Limits>>,
        enabled: Rc<Cell<bool>>,
        status_tx: Sender<ClipboardStatus>,
        devices: Arc<RwLock<DeviceStore>>,
    ) -> Self {
        let task = spawn_local(async move {
            let mut status = ClipboardStatus {
                enabled: enabled.get(),
                ..Default::default()
            };
            let _ = status_tx.send(status.clone());

            let mut channels = Channels {
                from_listener,
                from_connection,
                peer_events,
                incoming_caps,
            };
            // The first creation failure waits for the user (a denied portal
            // dialog must not loop); a backend that died later is restarted
            // automatically, which is what recovers from a portal restart.
            let mut restarted = false;

            loop {
                if !enabled.get() {
                    wait_for_enable(&mut channels, &enabled).await;
                }
                let retries = if restarted { 3 } else { 0 };
                let mut backend =
                    create_backend(&mut channels, &enabled, &mut status, &status_tx, retries).await;
                log::info!("using clipboard backend: {}", backend.backend_name());
                status.backend = Some(backend.backend_name().to_string());
                status.available = true;
                status.enabled = enabled.get();
                status.last_error = None;
                let _ = status_tx.send(status.clone());
                restarted = true;

                match run(
                    &mut backend,
                    client_manager.clone(),
                    sender.clone(),
                    replies.clone(),
                    &mut channels,
                    limits.clone(),
                    enabled.clone(),
                    &mut status,
                    &status_tx,
                    devices.clone(),
                )
                .await
                {
                    RunExit::Shutdown => break,
                    RunExit::BackendStopped => {
                        status.available = false;
                        status.last_error = Some("clipboard backend stopped; reconnecting".into());
                        let _ = status_tx.send(status.clone());
                        log::warn!("clipboard backend stopped, trying to restart it");
                    }
                }
            }
        });
        Self { task }
    }

    pub(crate) async fn terminate(&mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

/// Drop queued frames while no backend can consume them, so a peer cannot
/// grow the unbounded channel while clipboard sync is unavailable.
fn drain_frames<T>(rx: &mut Receiver<T>) {
    while let Some(Some(_)) = rx.recv().now_or_never() {}
}

fn local_origin() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ ((std::process::id() as u64) << 32)
}

#[allow(clippy::too_many_arguments)]
async fn run(
    backend: &mut Box<dyn Clipboard>,
    client_manager: ClientManager,
    sender: ConnectionSender,
    replies: Sender<ListenerCommand>,
    channels: &mut Channels,
    limits: Rc<Cell<Limits>>,
    enabled: Rc<Cell<bool>>,
    status: &mut ClipboardStatus,
    status_tx: &Sender<ClipboardStatus>,
    devices: Arc<RwLock<DeviceStore>>,
) -> RunExit {
    let routes: Rc<RefCell<HashMap<PeerId, Route>>> = Default::default();
    let (transport_tx, mut transport_rx) = channel::<(Route, Vec<u8>)>();
    let transport_sender = sender.clone();

    spawn_local(async move {
        while let Some((route, bytes)) = transport_rx.recv().await {
            match route {
                Route::Outgoing(handle) => {
                    if let Err(e) = transport_sender.send_raw(handle, &bytes).await {
                        log::debug!("clipboard: failed to send to client {handle}: {e}");
                    }
                }
                Route::Incoming(addr) => {
                    let _ = replies.send(ListenerCommand::Reply(addr, bytes));
                }
            }
        }
    });

    let sink = Sink {
        routes: routes.clone(),
        tx: transport_tx,
        client_manager: client_manager.clone(),
        devices,
    };
    // Shares the route table and device store with the sink: used to decide
    // whether a peer may receive clipboard content before a transfer starts.
    let policy_probe = sink.clone();
    let mut sync = ClipboardSync::new(sink, limits, local_origin());
    let mut addr_peers: HashMap<SocketAddr, PeerId> = HashMap::new();
    let mut known_outgoing: HashSet<ClientHandle> = HashSet::new();
    let mut next_peer: PeerId = 1 << 63;
    let mut last_enabled = enabled.get();
    // Peers that registered and still need the current clipboard pushed.
    let mut pending_push: HashSet<PeerId> = HashSet::new();
    let mut push_retry_at = Instant::now();
    let mut pending_apply: Option<PendingApply> = None;

    let mut ticker = tokio::time::interval(Duration::from_millis(15));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now_enabled = enabled.get();
                if now_enabled != last_enabled {
                    last_enabled = now_enabled;
                    status.enabled = now_enabled;
                    if !now_enabled {
                        // Tell a sender whose snapshot we were still writing
                        // that its transfer must be retried later.
                        cancel_pending_apply(&mut pending_apply, &mut sync);
                        for peer in sync.peers().collect::<Vec<_>>() {
                            sync.remove_peer(peer);
                        }
                        routes.borrow_mut().clear();
                        known_outgoing.clear();
                        addr_peers.clear();
                        pending_push.clear();
                        sync.reset();
                        sync.clear_error();
                        status.last_error = None;
                    }
                    let _ = status_tx.send(status.clone());
                }
                if !now_enabled {
                    continue;
                }

                let now = Instant::now();
                let active: HashSet<ClientHandle> =
                    client_manager.active_clients().into_iter().collect();
                for handle in known_outgoing.clone() {
                    if !active.contains(&handle) {
                        known_outgoing.remove(&handle);
                        routes.borrow_mut().remove(&handle);
                        pending_push.remove(&handle);
                        sync.remove_peer(handle);
                    }
                }
                for handle in active {
                    let connected = client_manager.active_addr(handle).is_some()
                        && client_manager
                            .peer_supports(handle, lan_mouse_proto::capabilities::CLIPBOARD);
                    if connected && known_outgoing.insert(handle) {
                        // An incoming connection may have registered an
                        // address-keyed peer before the outgoing Hello marked
                        // this client as connected. Migrate its state to the
                        // client handle instead of dropping in-flight
                        // transfers and acknowledgements.
                        if let Some(addr) = client_manager.active_addr(handle) {
                            for (stale_addr, stale_peer) in
                                take_address_peers_for_ip(&mut addr_peers, addr.ip())
                            {
                                routes.borrow_mut().remove(&stale_peer);
                                pending_push.remove(&stale_peer);
                                sync.migrate_peer(stale_peer, handle);
                                log::debug!(
                                    "clipboard: merged duplicate peer {stale_peer} ({stale_addr}) into client {handle}"
                                );
                            }
                        }
                        routes.borrow_mut().insert(handle, Route::Outgoing(handle));
                        sender.ensure_connected(handle);
                        // Register the peer even when it may not read our
                        // clipboard: its inbound content and acknowledgements
                        // still need a route, and frames are only accepted
                        // from registered peers.
                        sync.add_peer(handle);
                        let may_receive = policy_probe.peer_may_read(Route::Outgoing(handle));
                        sync.set_peer_may_receive(handle, may_receive);
                        if may_receive {
                            pending_push.insert(handle);
                        } else {
                            log::debug!(
                                "clipboard: peer may not read our clipboard, not offering it"
                            );
                        }
                        log::debug!(
                            "clipboard: client {handle} speaks the clipboard extension, syncing with it"
                        );
                    } else if !connected && known_outgoing.remove(&handle) {
                        routes.borrow_mut().remove(&handle);
                        pending_push.remove(&handle);
                        sync.remove_peer(handle);
                    } else if !connected
                        && client_manager.active_addr(handle).is_some()
                        && !client_manager
                            .peer_supports(handle, lan_mouse_proto::capabilities::CLIPBOARD)
                    {
                        log::debug!(
                            "clipboard: client {handle} does not advertise the clipboard extension"
                        );
                    }
                }

                // Peers that dialed us and advertised the clipboard extension:
                // without an outgoing link the listener reply path is their
                // only route. Register them so our clipboard is pushed and
                // their content/acks are accepted.
                let incoming_clipboard: Vec<SocketAddr> = {
                    let caps = channels.incoming_caps.read().expect("lock");
                    caps.iter()
                        .filter(|(_, capabilities)| **capabilities & CLIPBOARD != 0)
                        .map(|(addr, _)| *addr)
                        .collect()
                };
                for addr in incoming_clipboard {
                    let outgoing_known = client_manager
                        .get_client(addr)
                        .is_some_and(|handle| known_outgoing.contains(&handle));
                    if outgoing_known || addr_peers.contains_key(&addr) {
                        continue;
                    }
                    let peer = next_peer;
                    next_peer = next_peer.wrapping_add(1);
                    addr_peers.insert(addr, peer);
                    routes.borrow_mut().insert(peer, Route::Incoming(addr));
                    sync.add_peer(peer);
                    let may_receive = policy_probe.peer_may_read(Route::Incoming(addr));
                    sync.set_peer_may_receive(peer, may_receive);
                    if may_receive {
                        pending_push.insert(peer);
                    }
                    log::debug!(
                        "clipboard: incoming peer {addr} speaks the clipboard extension, syncing with it"
                    );
                }

                // Deliver the clipboard that was copied before the link came
                // up (or interrupted by a reconnect / backend restart). A
                // backend read failure is retried so a copy is not lost just
                // because the local clipboard is momentarily unreadable.
                if !pending_push.is_empty() && Instant::now() >= push_retry_at {
                    // A single read attempt (the Windows backend retries a
                    // locked clipboard internally); the resync itself is
                    // retried below so a long stall cannot block frame
                    // processing.
                    let read = backend.read().await;
                    match read {
                        Ok(snapshot) if !snapshot.is_empty() => {
                            let now = Instant::now();
                            for peer in pending_push.drain() {
                                sync.offer_current_to(peer, &snapshot, now);
                            }
                            push_retry_at = Instant::now();
                        }
                        Ok(_) => {
                            // The clipboard is empty: nothing to resync.
                            pending_push.clear();
                        }
                        Err(e) => {
                            log::debug!(
                                "clipboard: could not read the current clipboard for a new peer: {e}"
                            );
                            push_retry_at = Instant::now() + PUSH_RETRY_DELAY;
                        }
                    }
                }

                if let Some(snapshot) = sync.tick(now) {
                    // A newer remote copy supersedes a pending write.
                    if let Some(previous) = pending_apply.take() {
                        sync.on_apply_failed(&previous.snapshot);
                    }
                    pending_apply = Some(PendingApply::new(snapshot));
                }
                if pending_apply
                    .as_ref()
                    .is_some_and(|apply| apply.due(Instant::now()))
                {
                    let mut apply = pending_apply.take().expect("due");
                    match backend.write(&apply.snapshot).await {
                        Ok(()) => {
                            sync.on_applied(&apply.snapshot);
                            sync.clear_error();
                            status.applied = sync.applied_count();
                            status.last_error = None;
                            let _ = status_tx.send(status.clone());
                        }
                        Err(e) => {
                            log::warn!("clipboard: could not apply remote snapshot: {e}");
                            status.last_error = Some(e.to_string());
                            let _ = status_tx.send(status.clone());
                            if apply.on_failure(Instant::now()) {
                                log::debug!(
                                    "clipboard: retrying the failed clipboard write (attempt {})",
                                    apply.attempts
                                );
                                pending_apply = Some(apply);
                            } else {
                                log::warn!(
                                    "clipboard: giving up applying a remote snapshot; the sender will re-offer"
                                );
                                sync.on_apply_failed(&apply.snapshot);
                            }
                        }
                    }
                }
                if let Some(error) = sync.last_error().map(str::to_string) {
                    if status.last_error.as_deref() != Some(error.as_str()) {
                        status.last_error = Some(error);
                        let _ = status_tx.send(status.clone());
                    }
                }
            }
            change = backend.changed() => {
                match change {
                    Ok(Some(snapshot)) => {
                        status.detected += 1;
                        let bytes = snapshot.total_len();
                        let formats: Vec<&str> = snapshot.formats().map(|f| f.mime()).collect();
                        log::info!(
                            "clipboard: local change detected ({bytes} bytes, {formats:?})"
                        );
                        let _ = status_tx.send(status.clone());
                        if enabled.get() {
                            cancel_pending_apply(&mut pending_apply, &mut sync);
                            sync.on_local_change(snapshot, Instant::now());
                        }
                    }
                    Ok(None) => {
                        log::debug!("clipboard: local change stream ended");
                        if let Some(apply) = pending_apply.take() {
                            sync.on_apply_failed(&apply.snapshot);
                        }
                        return RunExit::BackendStopped;
                    }
                    Err(e) => {
                        log::warn!("clipboard: local change error: {e}");
                        // The OS notification was already consumed. A
                        // transient read failure (a clipboard manager holding
                        // the Windows clipboard open, a portal race) must not
                        // lose the user's copy: retry the read with a small
                        // bounded backoff.
                        let read = input_clipboard::read_retrying(
                            backend.as_mut(),
                            input_clipboard::CHANGE_READ_RETRIES,
                            input_clipboard::CHANGE_READ_BACKOFF,
                        )
                        .await;
                        if let Ok(snapshot) = read {
                            if !snapshot.is_empty() {
                                status.detected += 1;
                                let _ = status_tx.send(status.clone());
                                if enabled.get() {
                                    cancel_pending_apply(&mut pending_apply, &mut sync);
                                    sync.on_local_change(snapshot, Instant::now());
                                }
                            }
                        }
                    }
                }
            }
            incoming = channels.from_listener.recv() => {
                let Some((addr, frame)) = incoming else {
                    return RunExit::Shutdown;
                };
                if !enabled.get() {
                    continue;
                }
                // If we also have an outgoing connection to this peer, reuse
                // its client handle instead of creating a second peer. Acks
                // then travel over the outgoing connection.
                let peer = match client_manager
                    .get_client(addr)
                    .filter(|handle| known_outgoing.contains(handle))
                {
                    Some(handle) => handle,
                    None => *addr_peers.entry(addr).or_insert_with(|| {
                        let peer = next_peer;
                        next_peer = next_peer.wrapping_add(1);
                        routes.borrow_mut().insert(peer, Route::Incoming(addr));
                        sync.add_peer(peer);
                        let may_receive = policy_probe.peer_may_read(Route::Incoming(addr));
                        sync.set_peer_may_receive(peer, may_receive);
                        if may_receive {
                            // A peer that speaks the clipboard but has no
                            // outgoing link from us still gets the current
                            // clipboard.
                            pending_push.insert(peer);
                        }
                        log::debug!(
                            "clipboard: incoming peer {addr} sent a frame, syncing with it"
                        );
                        peer
                    }),
                };
                log::trace!("clipboard frame from incoming peer {addr}: {frame}");
                sync.handle_frame(peer, frame, Instant::now());
            }
            incoming = channels.from_connection.recv() => {
                let Some((handle, frame)) = incoming else {
                    return RunExit::Shutdown;
                };
                if enabled.get() {
                    sync.handle_frame(handle, frame, Instant::now());
                }
            }
            event = channels.peer_events.recv() => {
                match event {
                    Some(addr) => {
                        if let Some(peer) = addr_peers.remove(&addr) {
                            routes.borrow_mut().remove(&peer);
                            pending_push.remove(&peer);
                            sync.remove_peer(peer);
                        }
                    }
                    // The listener task is gone; the service is shutting down.
                    None => return RunExit::Shutdown,
                }
            }
            else => return RunExit::Shutdown,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        net::{IpAddr, Ipv4Addr},
        path::PathBuf,
        sync::{
            Arc, RwLock,
            atomic::{AtomicU16, AtomicU64, Ordering},
        },
        time::Duration,
    };

    use futures::StreamExt;
    use lan_mouse_ipc::Position as IpcPosition;
    use lan_mouse_proto::clipboard::{ClipboardFrame, MIME_TEXT};
    use tokio::task::LocalSet;
    use webrtc_dtls::crypto::Certificate;

    use super::*;
    use crate::{
        config::ConfigClient,
        connect::LanMouseConnection,
        crypto,
        listen::{LanMouseListener, ListenEvent},
    };

    static PORT_COUNTER: AtomicU16 = AtomicU16::new(0);
    static CERT_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn free_udp_port() -> u16 {
        loop {
            let offset = PORT_COUNTER.fetch_add(1, Ordering::Relaxed);
            let port = 31000 + (offset % 20000);
            if let Ok(socket) = std::net::UdpSocket::bind(("127.0.0.1", port)) {
                drop(socket);
                return port;
            }
        }
    }

    fn unique_cert(name: &str) -> Certificate {
        let dir = std::env::temp_dir().join("lan-mouse-clipboard-tests");
        std::fs::create_dir_all(&dir).expect("test dir");
        let id = CERT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path: PathBuf = dir.join(format!("{}-{id}-{name}.pem", std::process::id()));
        let _ = std::fs::remove_file(&path);
        crypto::generate_key_and_cert(&path).expect("certificate")
    }

    fn manager_for(port: u16) -> (ClientManager, ClientHandle) {
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

    /// Clipboard frames must traverse the real DTLS connection in both
    /// directions: offer/chunk out, ack back.
    #[tokio::test(flavor = "current_thread")]
    async fn clipboard_frames_traverse_dtls_in_both_directions() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let port = free_udp_port();
                let listener_cert = unique_cert("listener");
                let client_cert = unique_cert("client");
                let client_fp = crypto::certificate_fingerprint(&client_cert);

                let mut listener = LanMouseListener::new(
                    port,
                    listener_cert,
                    Arc::new(RwLock::new(HashMap::from([(
                        client_fp,
                        "peer".to_string(),
                    )]))),
                    Arc::new(RwLock::new(Vec::new())),
                    Default::default(),
                    Default::default(),
                    Arc::new(RwLock::new(true)),
                )
                .await
                .expect("listener");

                let (manager, handle) = manager_for(port);
                let mut connection = LanMouseConnection::new(client_cert, manager, 2000);
                let sender = connection.sender();
                sender.ensure_connected(handle);
                let mut clipboard_rx = connection.take_clipboard_rx().expect("clipboard receiver");

                let offer = ClipboardFrame::Offer {
                    serial: 7,
                    origin: 0xabcd,
                    hash: [0x11; 32],
                    items: vec![(MIME_TEXT, 12)],
                };

                let mut sent = false;
                for _ in 0..100 {
                    sender.ensure_connected(handle);
                    if sender.send_raw(handle, &offer.encode()).await.is_ok() {
                        sent = true;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                assert!(sent, "could not establish the connection");

                let (received, addr) = loop {
                    let event = tokio::time::timeout(Duration::from_secs(10), listener.next())
                        .await
                        .expect("timed out")
                        .expect("listener closed");
                    match event {
                        ListenEvent::Clipboard { frame, addr } => break (frame, addr),
                        ListenEvent::Accept { .. }
                        | ListenEvent::Msg { .. }
                        | ListenEvent::Closed { .. } => {}
                        ListenEvent::AutoAuthorized { .. } => {}
                        ListenEvent::Rejected { fingerprint, .. } => {
                            panic!("unexpected rejection: {fingerprint}")
                        }
                    }
                };
                assert_eq!(received, offer);

                let ack = ClipboardFrame::Ack {
                    serial: 7,
                    mime: MIME_TEXT,
                    base: 0,
                    bitmap: 1,
                };
                listener.reply_raw(addr, &ack.encode()).await;

                let (peer, frame) =
                    tokio::time::timeout(Duration::from_secs(10), clipboard_rx.recv())
                        .await
                        .expect("timed out waiting for the ack")
                        .expect("channel closed");
                assert_eq!(peer, handle);
                assert_eq!(frame, ack);

                listener.terminate().await;
            })
            .await;
    }

    /// A failed clipboard write is retried with a growing, bounded backoff so a
    /// transient portal/Windows failure does not lose the remote copy.
    #[test]
    fn failed_clipboard_writes_are_retried_with_a_bounded_backoff() {
        let snapshot = ClipboardSnapshot::from_items([(
            input_clipboard::Format::Text,
            b"remote copy".to_vec(),
        )]);
        let mut apply = PendingApply::new(snapshot);
        let t0 = Instant::now();
        assert!(apply.due(t0), "the first write is attempted immediately");

        let mut now = t0;
        let mut failures = 0;
        loop {
            failures += 1;
            if !apply.on_failure(now) {
                break;
            }
            assert!(
                !apply.due(now),
                "the retry must wait for the backoff, not spin"
            );
            assert!(apply.next_at > now);
            now = apply.next_at;
            assert!(apply.due(now), "the retry becomes due at next_at");
        }
        assert_eq!(
            failures,
            PendingApply::MAX_ATTEMPTS,
            "the retries are bounded"
        );
        assert!(
            now.duration_since(t0) <= Duration::from_secs(6),
            "the apply retry budget stays short"
        );
    }

    #[test]
    fn address_peers_of_the_same_ip_are_merged_into_the_client_handle() {
        let mut peers: HashMap<SocketAddr, PeerId> = HashMap::new();
        let first: SocketAddr = "10.0.0.2:4001".parse().expect("addr");
        let second: SocketAddr = "10.0.0.2:4002".parse().expect("addr");
        let other: SocketAddr = "10.0.0.3:4001".parse().expect("addr");
        peers.insert(first, 10);
        peers.insert(second, 11);
        peers.insert(other, 12);

        let mut removed =
            take_address_peers_for_ip(&mut peers, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)));
        removed.sort_by_key(|(addr, _)| *addr);
        assert_eq!(removed, vec![(first, 10), (second, 11)]);
        assert_eq!(peers.len(), 1);
        assert_eq!(peers.get(&other), Some(&12));
    }

    /// A new local copy supersedes a remote snapshot whose write is still
    /// pending: retrying that write later would overwrite the user's newer
    /// copy, so the pending apply must be cancelled (and its sender told to
    /// re-offer).
    #[test]
    fn a_new_local_copy_cancels_a_pending_apply() {
        use std::{cell::Cell, rc::Rc};

        use futures::{FutureExt, StreamExt};
        use input_clipboard::sync::ClipboardSync;

        let routes: Rc<RefCell<HashMap<PeerId, Route>>> = Default::default();
        let (tx, mut rx) = local_channel::mpsc::channel::<(Route, Vec<u8>)>();
        let sink = Sink {
            routes: routes.clone(),
            tx,
            client_manager: ClientManager::default(),
            devices: Arc::new(RwLock::new(DeviceStore::default())),
        };
        let mut sync = ClipboardSync::new(
            sink,
            Rc::new(Cell::new(input_clipboard::Limits::default())),
            0x1234,
        );
        sync.add_peer(2);
        routes.borrow_mut().insert(2, Route::Outgoing(2));
        let now = Instant::now();

        // A remote transfer completed, but its write is still pending.
        let copy = ClipboardSnapshot::from_items([(
            input_clipboard::Format::Text,
            b"remote copy".to_vec(),
        )]);
        let data = copy
            .get(input_clipboard::Format::Text)
            .expect("text")
            .to_vec();
        sync.handle_frame(
            2,
            ClipboardFrame::Offer {
                serial: 7,
                origin: 0x99,
                hash: copy.hash(),
                items: copy.wire_items(),
            },
            now,
        );
        sync.handle_frame(
            2,
            ClipboardFrame::Chunk {
                serial: 7,
                mime: MIME_TEXT,
                offset: 0,
                total: data.len() as u32,
                data,
            },
            now,
        );
        let completed = sync.tick(now).expect("complete transfer");
        let mut pending = Some(PendingApply::new(completed));

        cancel_pending_apply(&mut pending, &mut sync);
        assert!(pending.is_none(), "the pending write must be abandoned");
        let (route, bytes) = rx
            .next()
            .now_or_never()
            .flatten()
            .expect("the sender is told to abort");
        assert!(matches!(route, Route::Outgoing(2)));
        assert_eq!(
            ClipboardFrame::decode(&bytes).expect("frame"),
            ClipboardFrame::Cancel { serial: 7 },
            "the sender must be cancelled so it re-offers later"
        );
    }
}
