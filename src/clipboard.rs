//! Clipboard sync task.
//!
//! Bridges the platform-independent [`input_clipboard`] state machine to the
//! network: outgoing frames go through the same DTLS connection used for
//! input, incoming frames arrive from the listener (peer dialed us) or from
//! the outgoing connection (we dialed the peer).

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures::FutureExt;
use input_clipboard::{
    Clipboard, Limits,
    sync::{ClipboardSync, FrameSink, PeerId},
};
use lan_mouse_ipc::{ClientHandle, ClipboardStatus};
use lan_mouse_proto::clipboard::ClipboardFrame;
use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::task::{JoinHandle, spawn_local};

use crate::{client::ClientManager, connect::ConnectionSender, listen::ListenerCommand};

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
}

impl FrameSink for Sink {
    fn send(&self, peer: PeerId, frame: ClipboardFrame) {
        let route = self.routes.borrow().get(&peer).copied();
        if let Some(route) = route {
            let _ = self.tx.send((route, frame.encode()));
        }
    }
}

pub(crate) struct ClipboardTask {
    task: JoinHandle<()>,
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
        mut from_listener: Receiver<(SocketAddr, ClipboardFrame)>,
        mut from_connection: Receiver<(ClientHandle, ClipboardFrame)>,
        mut peer_events: Receiver<SocketAddr>,
        limits: Rc<Cell<Limits>>,
        enabled: Rc<Cell<bool>>,
        status_tx: Sender<ClipboardStatus>,
    ) -> Self {
        let task = spawn_local(async move {
            let mut status = ClipboardStatus {
                enabled: enabled.get(),
                ..Default::default()
            };
            let _ = status_tx.send(status.clone());

            let backend = loop {
                while !enabled.get() {
                    drain_frames(&mut from_listener);
                    drain_frames(&mut from_connection);
                    drain_frames(&mut peer_events);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                match input_clipboard::default_backend().await {
                    Ok(backend) => break backend,
                    Err(e) => {
                        status.enabled = enabled.get();
                        status.available = false;
                        status.last_error = Some(e.to_string());
                        log::info!("clipboard backend unavailable: {e}");
                        let _ = status_tx.send(status.clone());
                        // Retry only after the user toggles the clipboard off
                        // and on again, so a denied portal does not loop.
                        while enabled.get() {
                            drain_frames(&mut from_listener);
                            drain_frames(&mut from_connection);
                            drain_frames(&mut peer_events);
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            };
            log::info!("using clipboard backend: {}", backend.backend_name());
            status.backend = Some(backend.backend_name().to_string());
            status.available = true;
            status.enabled = enabled.get();
            status.last_error = None;
            let _ = status_tx.send(status.clone());

            run(
                backend,
                client_manager,
                sender,
                replies,
                from_listener,
                from_connection,
                peer_events,
                limits,
                enabled,
                status,
                status_tx,
            )
            .await;
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
async fn run<C: Clipboard>(
    mut backend: C,
    client_manager: ClientManager,
    sender: ConnectionSender,
    replies: Sender<ListenerCommand>,
    mut from_listener: Receiver<(SocketAddr, ClipboardFrame)>,
    mut from_connection: Receiver<(ClientHandle, ClipboardFrame)>,
    mut peer_events: Receiver<SocketAddr>,
    limits: Rc<Cell<Limits>>,
    enabled: Rc<Cell<bool>>,
    mut status: ClipboardStatus,
    status_tx: Sender<ClipboardStatus>,
) {
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
    };
    let mut sync = ClipboardSync::new(sink, limits, local_origin());
    let mut addr_peers: HashMap<SocketAddr, PeerId> = HashMap::new();
    let mut known_outgoing: HashSet<ClientHandle> = HashSet::new();
    let mut next_peer: PeerId = 1 << 63;
    let mut last_enabled = enabled.get();

    let mut ticker = tokio::time::interval(Duration::from_millis(50));
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
                        for peer in sync.peers().collect::<Vec<_>>() {
                            sync.remove_peer(peer);
                        }
                        routes.borrow_mut().clear();
                        known_outgoing.clear();
                        addr_peers.clear();
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
                        sync.remove_peer(handle);
                    }
                }
                for handle in active {
                    let connected = client_manager.active_addr(handle).is_some()
                        && client_manager
                            .peer_supports(handle, lan_mouse_proto::capabilities::CLIPBOARD);
                    if connected && known_outgoing.insert(handle) {
                        routes.borrow_mut().insert(handle, Route::Outgoing(handle));
                        sender.ensure_connected(handle);
                        sync.add_peer(handle);
                        // Make the clipboard that was copied before the link
                        // came up available on the peer as well.
                        if let Ok(snapshot) = backend.read().await {
                            sync.on_local_change(snapshot, now);
                        }
                    } else if !connected && known_outgoing.remove(&handle) {
                        routes.borrow_mut().remove(&handle);
                        sync.remove_peer(handle);
                    }
                }

                if let Some(snapshot) = sync.tick(now) {
                    if let Err(e) = backend.write(&snapshot).await {
                        log::warn!("clipboard: could not apply remote snapshot: {e}");
                        status.last_error = Some(e.to_string());
                        let _ = status_tx.send(status.clone());
                    } else {
                        sync.on_applied(&snapshot);
                        sync.clear_error();
                        status.applied = sync.applied_count();
                        status.last_error = None;
                        let _ = status_tx.send(status.clone());
                    }
                } else if let Some(error) = sync.last_error().map(str::to_string) {
                    if status.last_error.as_deref() != Some(error.as_str()) {
                        status.last_error = Some(error);
                        let _ = status_tx.send(status.clone());
                    }
                }
            }
            change = backend.changed() => {
                match change {
                    Ok(Some(snapshot)) => {
                        if enabled.get() {
                            sync.on_local_change(snapshot, Instant::now());
                        }
                    }
                    Ok(None) => {
                        log::debug!("clipboard: local change stream ended");
                        status.last_error = Some("clipboard backend stopped".into());
                        let _ = status_tx.send(status.clone());
                        break;
                    }
                    Err(e) => log::warn!("clipboard: local change error: {e}"),
                }
            }
            incoming = from_listener.recv() => {
                let Some((addr, frame)) = incoming else {
                    break;
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
                        peer
                    }),
                };
                log::trace!("clipboard frame from incoming peer {addr}: {frame}");
                sync.handle_frame(peer, frame, Instant::now());
            }
            incoming = from_connection.recv() => {
                let Some((handle, frame)) = incoming else {
                    break;
                };
                if enabled.get() {
                    sync.handle_frame(handle, frame, Instant::now());
                }
            }
            event = peer_events.recv() => {
                if let Some(addr) = event {
                    if let Some(peer) = addr_peers.remove(&addr) {
                        routes.borrow_mut().remove(&peer);
                        sync.remove_peer(peer);
                    }
                }
            }
            else => break,
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
                        ListenEvent::Accept { .. } | ListenEvent::Msg { .. } => {}
                        ListenEvent::Rejected { fingerprint } => {
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
}
