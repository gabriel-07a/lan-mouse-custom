use futures::{Stream, StreamExt};
use lan_mouse_proto::{
    MAX_EVENT_SIZE, ProtoEvent,
    clipboard::{ClipboardFrame, MAX_CLIPBOARD_FRAME, is_clipboard_frame},
};
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::CertificateDer;
use std::{
    cell::RefCell,
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    rc::Rc,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use thiserror::Error;
use tokio::task::{JoinHandle, spawn_local};
use webrtc_dtls::{
    config::{ClientAuthType::RequireAnyClientCert, Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::{Conn, Error, conn::Listener};

use crate::{crypto, devices::DeviceStore, trusted::AddressRule};
use input_event::{Event as InputEvent, PointerEvent};
use lan_mouse_ipc::{Capabilities, DirectionPolicy};

#[derive(Error, Debug)]
pub enum ListenerCreationError {
    #[error(transparent)]
    WebrtcUtil(#[from] webrtc_util::Error),
    #[error(transparent)]
    WebrtcDtls(#[from] webrtc_dtls::Error),
}

type ArcConn = Arc<dyn Conn + Send + Sync>;

/// Accepted incoming transports, keyed by the *logical* address the rest of
/// the program knows the peer by (see [`SessionMeta::logical_addr`]).
type ConnMap = Rc<RefCell<HashMap<SocketAddr, ArcConn>>>;

/// Bookkeeping for one accepted peer identity (certificate fingerprint).
///
/// Identity is the fingerprint, never the address: the same peer may reach us
/// over IPv4 or IPv6 (and with a new source port on every reconnect). Keeping
/// the address the peer was first accepted on stable means:
/// - the service keeps one client/incoming entry per peer instead of one per
///   address family;
/// - the UI does not alternate between `192.168.x.x` and `2804:...`;
/// - session state (pressed keys, capture zone) is not torn down just because
///   the peer switched families.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionMeta {
    /// Address used by the service and by replies (`reply`/`reply_raw`).
    logical_addr: SocketAddr,
    /// Monotonic id of the current transport for this identity; a stale
    /// `read_loop` must not remove or close the connection that replaced it.
    generation: u64,
}

type SessionMap = Rc<RefCell<HashMap<String, SessionMeta>>>;

/// Everything a live incoming transport needs while its `read_loop` runs.
///
/// Grouped in a struct because these values travel together for the whole
/// session: the transport, its identity/generation in the session table, the
/// event channel and the permission maps consulted per event. Nine positional
/// arguments were unreadable and tripped `clippy::too_many_arguments`.
struct ReadLoopCtx {
    conns: ConnMap,
    sessions: SessionMap,
    generation: u64,
    fingerprint: String,
    logical_addr: SocketAddr,
    conn: ArcConn,
    dtls_tx: Sender<ListenEvent>,
    devices: Arc<RwLock<DeviceStore>>,
    authorized: Arc<RwLock<HashMap<String, String>>>,
}

/// What the accept task must do with a freshly authenticated connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AcceptPlan {
    /// Address the service is told about (or already knows).
    logical_addr: SocketAddr,
    /// `true` when this fingerprint already had a live session; the new
    /// transport replaces the old one under the same logical address, so no
    /// `Accept`/`Connected` event is emitted again.
    replacement: bool,
}

/// Decide how a newly authenticated incoming connection maps to a peer
/// identity and update the session table accordingly.
///
/// Split out from the async accept task so the identity/dedupe rules are unit
/// testable. Must be called without awaiting (it is a single synchronous
/// step on the single-threaded runtime, which keeps concurrent handshakes from
/// both enrolling).
fn plan_accept(
    sessions: &mut HashMap<String, SessionMeta>,
    fingerprint: &str,
    addr: SocketAddr,
    generation: u64,
) -> AcceptPlan {
    match sessions.get(fingerprint) {
        Some(existing) => {
            let logical_addr = existing.logical_addr;
            sessions.insert(
                fingerprint.to_string(),
                SessionMeta {
                    logical_addr,
                    generation,
                },
            );
            AcceptPlan {
                logical_addr,
                replacement: true,
            }
        }
        None => {
            sessions.insert(
                fingerprint.to_string(),
                SessionMeta {
                    logical_addr: addr,
                    generation,
                },
            );
            AcceptPlan {
                logical_addr: addr,
                replacement: false,
            }
        }
    }
}

/// Whether `fingerprint`'s current transport is still `generation`.
fn session_is_current(
    sessions: &HashMap<String, SessionMeta>,
    fingerprint: &str,
    generation: u64,
) -> bool {
    sessions
        .get(fingerprint)
        .is_some_and(|meta| meta.generation == generation)
}

/// Remove the session/transport for `fingerprint` if it is still `generation`.
///
/// Returns `true` when the caller is responsible for emitting
/// [`ListenEvent::Closed`]. Stale transports (already replaced) return `false`
/// and must stay silent, otherwise the service would tear down the live
/// session of the address family that replaced them.
fn finish_session(
    conns: &ConnMap,
    sessions: &SessionMap,
    fingerprint: &str,
    logical_addr: SocketAddr,
    generation: u64,
    conn: &ArcConn,
) -> bool {
    if !session_is_current(&sessions.borrow(), fingerprint, generation) {
        return false;
    }
    sessions.borrow_mut().remove(fingerprint);
    let mut conns = conns.borrow_mut();
    // Only remove the transport if it is still this one; a replacement may
    // have been installed under the same logical address.
    let ours = conns
        .get(&logical_addr)
        .is_some_and(|current| Arc::ptr_eq(current, conn));
    if ours {
        conns.remove(&logical_addr);
    }
    ours
}

/// Commands sent from other tasks to the listener task.
pub(crate) enum ListenerCommand {
    /// Send an already encoded frame to a connected peer.
    Reply(SocketAddr, Vec<u8>),
}

pub(crate) enum ListenEvent {
    Msg {
        event: ProtoEvent,
        addr: SocketAddr,
    },
    Clipboard {
        frame: ClipboardFrame,
        addr: SocketAddr,
    },
    Accept {
        addr: SocketAddr,
        fingerprint: String,
    },
    Rejected {
        addr: SocketAddr,
        fingerprint: String,
    },
    AutoAuthorized {
        addr: SocketAddr,
        fingerprint: String,
        /// The peer was accepted because the pairing window was open (in
        /// contrast to an address rule).
        from_pairing: bool,
        /// permissions granted by the address rule that adopted the device
        incoming: DirectionPolicy,
        outgoing: DirectionPolicy,
    },
    /// An incoming DTLS connection was closed.
    Closed {
        addr: SocketAddr,
    },
}

/// Per-IP handshake budget: a hostile LAN peer cannot force an unbounded
/// number of certificate verifications/tasks. Well above anything a normal
/// reconnect storm produces (the connect side backs off to 5 s).
const HANDSHAKES_PER_IP: u32 = 20;
const HANDSHAKE_WINDOW: Duration = Duration::from_secs(60);
/// Hard cap on simultaneous accepted connections.
const MAX_LIVE_CONNECTIONS: usize = 128;

/// Simple sliding-window attempt limiter, keyed by peer address.
struct AttemptLimiter {
    window: Duration,
    max: u32,
    attempts: HashMap<IpAddr, (u32, std::time::Instant)>,
}

impl AttemptLimiter {
    fn new(max: u32, window: Duration) -> Self {
        Self {
            window,
            max,
            attempts: HashMap::new(),
        }
    }

    /// Record an attempt and report whether it is within the budget.
    fn allow(&mut self, ip: IpAddr) -> bool {
        let now = std::time::Instant::now();
        let entry = self.attempts.entry(ip).or_insert((0, now));
        if now.duration_since(entry.1) >= self.window {
            *entry = (0, now);
        }
        entry.0 = entry.0.saturating_add(1);
        entry.0 <= self.max
    }

    /// Drop stale entries so the map cannot grow with spoofed addresses.
    fn prune(&mut self) {
        let now = std::time::Instant::now();
        self.attempts
            .retain(|_, (_, last)| now.duration_since(*last) < self.window);
    }
}

pub(crate) struct LanMouseListener {
    listen_rx: Receiver<ListenEvent>,
    listen_tx: Sender<ListenEvent>,
    listen_task: JoinHandle<()>,
    /// The single live transport per logical peer address.
    conns: ConnMap,
    /// Accepted peer identities (certificate fingerprints).
    sessions: SessionMap,
    request_port_change: Sender<u16>,
    port_changed: Receiver<Result<u16, ListenerCreationError>>,
}

type VerifyPeerCertificateFn = Arc<
    dyn (Fn(&[Vec<u8>], &[CertificateDer<'static>]) -> Result<(), webrtc_dtls::Error>)
        + Send
        + Sync,
>;

/// An open pairing window: devices accepted while it lasts get exactly these
/// permissions, and the window is consumed by the first accepted device.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PairingWindow {
    pub until: std::time::Instant,
    pub incoming: DirectionPolicy,
    pub outgoing: DirectionPolicy,
}

impl PairingWindow {
    pub(crate) fn is_open(&self) -> bool {
        std::time::Instant::now() < self.until
    }
}

impl LanMouseListener {
    pub(crate) async fn new(
        port: u16,
        cert: Certificate,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
        trusted_nets: Arc<RwLock<Vec<AddressRule>>>,
        pairing_until: Arc<RwLock<Option<PairingWindow>>>,
        devices: Arc<RwLock<DeviceStore>>,
        accept_incoming: Arc<RwLock<bool>>,
    ) -> Result<Self, ListenerCreationError> {
        let (listen_tx, listen_rx) = channel();
        let (request_port_change, mut request_port_change_rx) = channel();
        let (port_changed_tx, port_changed) = channel();
        let cfg = Config {
            certificates: vec![cert.clone()],
            extended_master_secret: ExtendedMasterSecretType::Require,
            client_auth: RequireAnyClientCert,
            ..Default::default()
        };

        let mut listener = bind_listener(port).await?;

        let conns: ConnMap = Default::default();
        let sessions: SessionMap = Default::default();
        // Monotonic transport id shared by the accept task and every
        // `read_loop`, replacing the old "first matching address wins"
        // bookkeeping.
        let transport_generation = Rc::new(std::cell::Cell::new(0u64));

        let conns_clone = conns.clone();
        let sessions_clone = sessions.clone();
        let generation_clone = transport_generation.clone();
        let listen_task: JoinHandle<()> = {
            let listen_tx = listen_tx.clone();
            let authorized = authorized_keys.clone();
            let trusted = trusted_nets.clone();
            let pairing = pairing_until.clone();
            let devices = devices.clone();
            let accept_incoming = accept_incoming.clone();
            spawn_local(async move {
                // Bound the number of half-open handshakes so stray UDP
                // traffic cannot spawn unbounded tasks.
                let handshakes = Arc::new(tokio::sync::Semaphore::new(32));
                let mut limiter = AttemptLimiter::new(HANDSHAKES_PER_IP, HANDSHAKE_WINDOW);
                let mut rate_limited = 0u64;
                loop {
                    tokio::select! {
                        c = listener.accept() => match c {
                            Ok((conn, addr)) => {
                                let addr = normalize_addr(addr);
                                if limiter.attempts.len() > 1024 {
                                    limiter.prune();
                                }
                                if !limiter.allow(addr.ip()) {
                                    rate_limited = rate_limited.saturating_add(1);
                                    if rate_limited == 1 || rate_limited.is_multiple_of(100) {
                                        log::warn!(
                                            "rate limiting handshakes from {} ({rate_limited} dropped so far)",
                                            addr.ip()
                                        );
                                    }
                                    let _ = conn.close().await;
                                    continue;
                                }
                                if conns_clone.borrow().len() >= MAX_LIVE_CONNECTIONS {
                                    log::warn!(
                                        "too many live connections, dropping handshake from {addr}"
                                    );
                                    let _ = conn.close().await;
                                    continue;
                                }
                                let Ok(permit) = handshakes.clone().try_acquire_owned() else {
                                    log::warn!("too many handshakes in flight, dropping {addr}");
                                    // Close the accepted transport like the
                                    // other rejection paths: dropping it without
                                    // closing leaked the socket/bookkeeping.
                                    let _ = conn.close().await;
                                    continue;
                                };
                                let listen_tx = listen_tx.clone();
                                let authorized = authorized.clone();
                                let trusted = trusted.clone();
                                let pairing = pairing.clone();
                                let devices = devices.clone();
                                let accept_incoming = accept_incoming.clone();
                                let conns = conns_clone.clone();
                                let sessions = sessions_clone.clone();
                                let generation = generation_clone.clone();
                                let cfg = cfg.clone();
                                spawn_local(async move {
                                    let _permit = permit;
                                    let recorded: Arc<Mutex<Option<String>>> = Default::default();
                                    let auto_source: Arc<Mutex<Option<AutoSource>>> =
                                        Default::default();
                                    let mut cfg = cfg;
                                    cfg.verify_peer_certificate = Some(per_connection_verify(
                                        authorized.clone(),
                                        trusted,
                                        pairing.clone(),
                                        devices.clone(),
                                        accept_incoming.clone(),
                                        addr,
                                        recorded.clone(),
                                        auto_source.clone(),
                                    ));
                                    // `false` = server role, as in the
                                    // upstream listener wrapper.
                                    match DTLSConn::new(conn, cfg, false, None).await {
                                        Ok(dtls) => {
                                            let dtls = Arc::new(dtls);
                                            let fingerprint = dtls
                                                .connection_state()
                                                .await
                                                .peer_certificates
                                                .first()
                                                .map(|cert| crypto::generate_fingerprint(cert))
                                                .unwrap_or_default();
                                            let transport_id = generation.get().wrapping_add(1);
                                            generation.set(transport_id);
                                            let generation = transport_id;
                                            // Identity is the certificate fingerprint, not
                                            // the address. Enrol/replace synchronously so
                                            // two concurrent handshakes of the same peer
                                            // cannot both create a logical session.
                                            let plan = {
                                                let mut sessions = sessions.borrow_mut();
                                                plan_accept(
                                                    &mut sessions,
                                                    &fingerprint,
                                                    addr,
                                                    generation,
                                                )
                                            };
                                            let logical_addr = plan.logical_addr;
                                            let replaced = conns
                                                .borrow_mut()
                                                .insert(logical_addr, dtls.clone());
                                            if let Some(old) = replaced {
                                                // Deduplication: the logical session
                                                // stays, only the transport is replaced.
                                                let _ = old.close().await;
                                            }
                                            if plan.replacement {
                                                log::info!(
                                                    "peer {fingerprint} reconnected from {addr}; \
                                                     keeping logical address {logical_addr}"
                                                );
                                            } else {
                                                log::info!(
                                                    "dtls client connected, ip: {addr} ({fingerprint})"
                                                );
                                                // Auto-authorization (allowlisted
                                                // address or pairing window) is
                                                // only persisted once the
                                                // handshake actually succeeded, so
                                                // a failed/aborted handshake
                                                // cannot leave an orphaned pin.
                                                let mut adoption = None;
                                                let mut pairing_lost_race = false;
                                                if let Some(source) =
                                                    auto_source.lock().expect("lock").take()
                                                {
                                                    match source {
                                                        AutoSource::TrustedAddress {
                                                            incoming,
                                                            outgoing,
                                                        } => {
                                                            adoption = Some((
                                                                format!(
                                                                    "trusted {}",
                                                                    logical_addr.ip()
                                                                ),
                                                                incoming,
                                                                outgoing,
                                                                false,
                                                            ));
                                                        }
                                                        AutoSource::Pairing {
                                                            incoming,
                                                            outgoing,
                                                        } => {
                                                            // Single use: consume the
                                                            // window atomically here,
                                                            // after the handshake. Two
                                                            // concurrent handshakes may
                                                            // both have seen it open in
                                                            // the verify callback; only
                                                            // the one that takes it may
                                                            // adopt the device.
                                                            if pairing
                                                                .write()
                                                                .expect("lock")
                                                                .take()
                                                                .is_some()
                                                            {
                                                                adoption = Some((
                                                                    format!(
                                                                        "paired {}",
                                                                        logical_addr.ip()
                                                                    ),
                                                                    incoming,
                                                                    outgoing,
                                                                    true,
                                                                ));
                                                            } else {
                                                                pairing_lost_race = true;
                                                            }
                                                        }
                                                    }
                                                }
                                                if pairing_lost_race {
                                                    // A concurrent handshake
                                                    // consumed the window first:
                                                    // this device was never
                                                    // authorized (and may never
                                                    // become a full-access pin).
                                                    // Drop the transport and the
                                                    // session bookkeeping.
                                                    log::warn!(
                                                        "rejecting {logical_addr} ({fingerprint}): pairing window already consumed"
                                                    );
                                                    let transport: ArcConn = dtls.clone();
                                                    let _ = finish_session(
                                                        &conns,
                                                        &sessions,
                                                        &fingerprint,
                                                        logical_addr,
                                                        generation,
                                                        &transport,
                                                    );
                                                    let _ = dtls.close().await;
                                                    return;
                                                }
                                                if let Some((
                                                    description,
                                                    incoming,
                                                    outgoing,
                                                    from_pairing,
                                                )) = adoption
                                                {
                                                    authorized
                                                        .write()
                                                        .expect("lock")
                                                        .insert(fingerprint.clone(), description);
                                                    log::info!(
                                                        "auto-authorized {logical_addr} ({fingerprint})"
                                                    );
                                                    let _ = listen_tx.send(
                                                        ListenEvent::AutoAuthorized {
                                                            addr: logical_addr,
                                                            fingerprint: fingerprint.clone(),
                                                            from_pairing,
                                                            incoming,
                                                            outgoing,
                                                        },
                                                    );
                                                }
                                                let _ = listen_tx.send(ListenEvent::Accept {
                                                    addr: logical_addr,
                                                    fingerprint: fingerprint.clone(),
                                                });
                                            }
                                            spawn_local(read_loop(ReadLoopCtx {
                                                conns,
                                                sessions,
                                                generation,
                                                fingerprint,
                                                logical_addr,
                                                conn: dtls,
                                                dtls_tx: listen_tx,
                                                devices,
                                                authorized,
                                            }));
                                        }
                                        Err(e) => {
                                            if matches!(
                                                e,
                                                webrtc_dtls::Error::ErrVerifyDataMismatch
                                            ) {
                                                if let Some(fingerprint) = recorded.lock().expect("lock").take() {
                                                    let _ = listen_tx.send(ListenEvent::Rejected { addr, fingerprint });
                                                }
                                            } else {
                                                log::debug!("handshake with {addr} failed: {e}");
                                            }
                                        }
                                    }
                                });
                            }
                            Err(e) => log::warn!("accept: {e:?}"),
                        },
                        port = request_port_change_rx.recv() => {
                            let port = port.expect("channel closed");
                            match bind_listener(port).await {
                                Ok(new_listener) => {
                                    let _ = listener.close().await;
                                    listener = new_listener;
                                    port_changed_tx.send(Ok(port)).expect("channel closed");
                                }
                                Err(e) => {
                                    log::warn!("unable to change port: {e}");
                                    port_changed_tx.send(Err(e.into())).expect("channel closed");
                                }
                            };
                        },
                    };
                }
            })
        };

        Ok(Self {
            conns,
            sessions,
            listen_rx,
            listen_tx,
            listen_task,
            port_changed,
            request_port_change,
        })
    }

    pub(crate) fn request_port_change(&mut self, port: u16) {
        self.request_port_change.send(port).expect("channel closed");
    }

    pub(crate) async fn port_changed(&mut self) -> Result<u16, ListenerCreationError> {
        self.port_changed.recv().await.expect("channel closed")
    }

    pub(crate) async fn terminate(&mut self) {
        self.listen_task.abort();
        let conns = self.conns.borrow().values().cloned().collect::<Vec<_>>();
        self.sessions.borrow_mut().clear();
        for conn in conns {
            let _ = conn.close().await;
        }
        self.listen_tx.close();
    }

    pub(crate) async fn reply(&self, addr: SocketAddr, event: ProtoEvent) {
        log::trace!("reply {event} >=>=>=>=>=> {addr}");
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        self.reply_raw(addr, &buf[..len]).await;
    }

    /// Send an already encoded frame (clipboard acks and chunks).
    ///
    /// The address identifies the logical session (one transport per accepted
    /// peer identity); a reconnect may have replaced the underlying transport
    /// while keeping the same logical address.
    pub(crate) async fn reply_raw(&self, addr: SocketAddr, bytes: &[u8]) {
        let conn = self.conns.borrow().get(&addr).cloned();
        if let Some(conn) = conn {
            let _ = conn.send(bytes).await;
        }
    }

    /// Close an active incoming connection.
    ///
    /// Used when a device is revoked while it is still connected: removing the
    /// fingerprint alone would leave the live DTLS session able to inject
    /// input until it drops on its own. The session is removed before the
    /// transport closes so the `read_loop` does not report a second close.
    pub(crate) async fn disconnect(&self, addr: SocketAddr) {
        let conn = self.conns.borrow_mut().remove(&addr);
        self.sessions
            .borrow_mut()
            .retain(|_, meta| meta.logical_addr != addr);
        if let Some(conn) = conn {
            log::info!("closing incoming connection from {addr}");
            let _ = conn.close().await;
            let _ = self.listen_tx.send(ListenEvent::Closed { addr });
        }
    }

    pub(crate) async fn get_certificate_fingerprint(&self, addr: SocketAddr) -> Option<String> {
        let conn = self.conns.borrow().get(&addr).cloned()?;
        let conn: &DTLSConn = conn.as_any().downcast_ref().expect("dtls conn");
        let certs = conn.connection_state().await.peer_certificates;
        let cert = certs.first()?;
        Some(crypto::generate_fingerprint(cert))
    }
}

impl Stream for LanMouseListener {
    type Item = ListenEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.listen_rx.poll_next_unpin(cx)
    }
}

/// Why an incoming certificate was accepted without an interactive prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoSource {
    /// The peer's address matched an address rule; the rule carries the
    /// permissions the adopted device gets.
    TrustedAddress {
        incoming: DirectionPolicy,
        outgoing: DirectionPolicy,
    },
    /// The user opened the pairing window, which accepts one new device with
    /// the permissions configured for the window.
    Pairing {
        incoming: DirectionPolicy,
        outgoing: DirectionPolicy,
    },
}

/// Build the certificate verifier for a single incoming connection.
///
/// It runs inside the DTLS handshake, which is the only place that knows both
/// the peer certificate *and* the address it came from. It only ever *reads*
/// state: auto-authorization is recorded here but persisted by the accept
/// task after the handshake succeeds, so an aborted handshake cannot leave an
/// orphaned pin.
#[allow(clippy::too_many_arguments)]
fn per_connection_verify(
    authorized: Arc<RwLock<HashMap<String, String>>>,
    trusted: Arc<RwLock<Vec<AddressRule>>>,
    pairing_until: Arc<RwLock<Option<PairingWindow>>>,
    devices: Arc<RwLock<DeviceStore>>,
    accept_incoming: Arc<RwLock<bool>>,
    addr: SocketAddr,
    recorded: Arc<Mutex<Option<String>>>,
    auto_source: Arc<Mutex<Option<AutoSource>>>,
) -> VerifyPeerCertificateFn {
    Arc::new(
        move |certs: &[Vec<u8>], _chains: &[CertificateDer<'static>]| {
            // Kiosk mode: no incoming control at all. Rejecting in the
            // certificate callback covers authorized devices, address rules
            // and the pairing window alike, and the peer only sees a
            // handshake failure.
            if !*accept_incoming.read().expect("lock") {
                log::debug!("rejecting incoming connection from {addr}: kiosk mode");
                return Err(webrtc_dtls::Error::ErrVerifyDataMismatch);
            }
            let Some(cert) = certs.first() else {
                return Err(webrtc_dtls::Error::ErrVerifyDataMismatch);
            };
            let fingerprint = crypto::generate_fingerprint(cert);
            *recorded.lock().expect("lock") = Some(fingerprint.clone());

            // A revoked device must stay revoked: the deny list wins over the
            // authorized map, the address allowlist and an open pairing window.
            // The check goes through the device store so an expiring deny stops
            // blocking as soon as its deadline passes.
            if devices.read().expect("lock").is_denied(&fingerprint) {
                log::warn!("rejecting revoked device {fingerprint} from {addr}");
                return Err(webrtc_dtls::Error::ErrVerifyDataMismatch);
            }

            if authorized.read().expect("lock").contains_key(&fingerprint) {
                return Ok(());
            }

            // Pairing mode trusts one new device for a short window, which
            // covers the case where the other machine has no mouse to approve.
            let pairing = pairing_until
                .read()
                .expect("lock")
                .as_ref()
                .filter(|window| window.is_open())
                .copied();
            if let Some(window) = pairing {
                *auto_source.lock().expect("lock") = Some(AutoSource::Pairing {
                    incoming: window.incoming,
                    outgoing: window.outgoing,
                });
                return Ok(());
            }

            // An address rule is trusted on first use; the fingerprint is
            // stored after the handshake so later connections match by
            // certificate instead of by address. The rule also decides the
            // permissions the adopted device gets.
            if let Some(rule) = trusted
                .read()
                .expect("lock")
                .iter()
                .copied()
                .find(|rule| rule.incoming.allowed && rule.contains(addr.ip()))
            {
                *auto_source.lock().expect("lock") = Some(AutoSource::TrustedAddress {
                    incoming: rule.incoming,
                    outgoing: rule.outgoing,
                });
                return Ok(());
            }

            Err(webrtc_dtls::Error::ErrVerifyDataMismatch)
        },
    )
}

/// Listen on an IPv4-only socket.
///
/// Lan Mouse deliberately does not accept or open IPv6 connections: all
/// supported setups (Windows, macOS and Linux) work over IPv4, and keeping one
/// family removes the address-flapping/duplicate-session failure modes that
/// occurred when both A and AAAA records were available.
async fn bind_listener(port: u16) -> Result<impl Listener, webrtc_util::Error> {
    let primary = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
    webrtc_util::conn::conn_udp_listener::listen(primary).await
}

/// Map IPv4-mapped IPv6 addresses (dual-stack sockets) back to IPv4, so
/// configured client addresses keep matching.
fn normalize_addr(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::V4(SocketAddrV4::new(v4, v6.port())),
            None => addr,
        },
        SocketAddr::V4(_) => addr,
    }
}

async fn read_loop(ctx: ReadLoopCtx) -> Result<(), Error> {
    let ReadLoopCtx {
        conns,
        sessions,
        generation,
        fingerprint,
        logical_addr,
        conn,
        dtls_tx,
        devices,
        authorized,
    } = ctx;
    let mut b = [0u8; MAX_CLIPBOARD_FRAME];
    let mut blocked = 0u64;
    let addr = logical_addr;

    loop {
        if !session_is_current(&sessions.borrow(), &fingerprint, generation) {
            // A newer transport replaced this one; it owns the logical
            // session now. Exit without emitting `Closed`.
            return Ok(());
        }
        let n = match conn.recv(&mut b).await {
            Ok(n) => n,
            Err(_) => break,
        };
        if !session_is_current(&sessions.borrow(), &fingerprint, generation) {
            return Ok(());
        }
        if n > 0 && is_clipboard_frame(b[0]) {
            let clipboard_policy = incoming_policy(&devices, &authorized, &fingerprint);
            if !(clipboard_policy.allowed && clipboard_policy.capabilities.allows_clipboard_write())
            {
                blocked = blocked.saturating_add(1);
                log::debug!(
                    "dropping clipboard frame from {addr}: device has no clipboard permission"
                );
                continue;
            }
            match ClipboardFrame::decode(&b[..n]) {
                Ok(frame) => {
                    if dtls_tx
                        .send(ListenEvent::Clipboard { frame, addr })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => log::debug!("ignoring undecodable clipboard frame from {addr}: {e}"),
            }
            continue;
        }
        match ProtoEvent::try_from(&b[..n]) {
            Ok(event) => {
                if !event_allowed(&devices, &authorized, &fingerprint, &event) {
                    blocked = blocked.saturating_add(1);
                    if blocked == 1 || blocked.is_multiple_of(1000) {
                        log::warn!(
                            "dropping input from {addr}: capability not granted \
                             ({blocked} blocked event(s) so far)"
                        );
                    }
                    continue;
                }
                if dtls_tx.send(ListenEvent::Msg { event, addr }).is_err() {
                    break;
                }
            }
            Err(e) => {
                // Skip the malformed/unknown datagram and keep
                // listening. Each DTLS recv returns one full
                // datagram, so a parse error here can't desync a
                // stream; the next call gets a fresh, framed
                // message. This makes the protocol forward-
                // compatible: a peer running a newer Lan Mouse
                // version can introduce additional event types
                // and old peers will simply ignore them rather
                // than dropping the connection.
                log::debug!("ignoring undecodable event from {addr}: {e}");
            }
        }
    }
    log::info!("dtls client disconnected {addr:?}");
    if finish_session(
        &conns,
        &sessions,
        &fingerprint,
        logical_addr,
        generation,
        &conn,
    ) {
        let _ = dtls_tx.send(ListenEvent::Closed { addr });
    }
    Ok(())
}

/// Incoming policy for a connection.
///
/// A fingerprint whose device record has not been registered yet (a device
/// that was just auto-authorized, or a pre-permissions test/legacy setup) is
/// only trusted when it is in the listener's authorized map; that keeps the
/// safe default (deny unknown devices) without dropping the first events of
/// a freshly authorized session.
fn incoming_policy(
    devices: &RwLock<DeviceStore>,
    authorized: &RwLock<HashMap<String, String>>,
    fingerprint: &str,
) -> lan_mouse_ipc::DirectionPolicy {
    let store = devices.read().expect("lock");
    if store.is_known(fingerprint) {
        return store.incoming(fingerprint);
    }
    drop(store);
    if authorized.read().expect("lock").contains_key(fingerprint) {
        lan_mouse_ipc::DirectionPolicy::allow_all()
    } else {
        lan_mouse_ipc::DirectionPolicy::deny()
    }
}

/// Whether the peer is allowed to inject `event` under its incoming policy.
fn event_allowed(
    devices: &RwLock<DeviceStore>,
    authorized: &RwLock<HashMap<String, String>>,
    fingerprint: &str,
    event: &ProtoEvent,
) -> bool {
    let policy = incoming_policy(devices, authorized, fingerprint);
    match event {
        ProtoEvent::Input(InputEvent::Pointer(
            PointerEvent::Motion { .. }
            | PointerEvent::Button { .. }
            | PointerEvent::Axis { .. }
            | PointerEvent::AxisDiscrete120 { .. },
        )) => policy.allows(Capabilities::POINTER),
        ProtoEvent::Input(InputEvent::Keyboard(_)) => policy.allows(Capabilities::KEYBOARD),
        ProtoEvent::Enter { .. } => {
            policy.allowed
                && (policy.capabilities.contains(Capabilities::POINTER)
                    || policy.capabilities.contains(Capabilities::KEYBOARD))
        }
        // Hand-off bookkeeping and keep-alives are always allowed.
        _ => true,
    }
}

#[cfg(test)]
mod limiter_tests {
    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
    }

    #[test]
    fn limiter_caps_attempts_per_ip() {
        let mut limiter = AttemptLimiter::new(2, Duration::from_secs(60));
        assert!(limiter.allow(ip(1)));
        assert!(limiter.allow(ip(1)));
        assert!(!limiter.allow(ip(1)), "third attempt is over budget");
    }

    #[test]
    fn limiter_isolates_addresses() {
        let mut limiter = AttemptLimiter::new(1, Duration::from_secs(60));
        assert!(limiter.allow(ip(1)));
        assert!(!limiter.allow(ip(1)));
        assert!(limiter.allow(ip(2)), "another peer has its own budget");
    }

    #[test]
    fn limiter_window_resets() {
        // A zero-length window resets every attempt, so nothing is blocked.
        let mut limiter = AttemptLimiter::new(1, Duration::ZERO);
        assert!(limiter.allow(ip(1)));
        assert!(limiter.allow(ip(1)));
    }

    #[test]
    fn prune_drops_stale_keys() {
        let mut limiter = AttemptLimiter::new(1, Duration::ZERO);
        assert!(limiter.allow(ip(1)));
        limiter.prune();
        assert!(limiter.attempts.is_empty());
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    fn addr(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse().expect("ip"), port)
    }

    #[test]
    fn first_connection_enrols_with_its_own_address() {
        let mut sessions = HashMap::new();
        let v4 = addr("192.168.1.5", 1000);
        let plan = plan_accept(&mut sessions, "fingerprint", v4, 1);
        assert_eq!(
            plan,
            AcceptPlan {
                logical_addr: v4,
                replacement: false,
            }
        );
        assert!(session_is_current(&sessions, "fingerprint", 1));
    }

    #[test]
    fn same_fingerprint_from_another_family_keeps_the_logical_address() {
        let mut sessions = HashMap::new();
        let v4 = addr("192.168.1.5", 1000);
        let v6 = addr("fd00::5", 2000);

        let first = plan_accept(&mut sessions, "fingerprint", v4, 1);
        assert!(!first.replacement);

        // The peer dialed us again over IPv6 (or reconnected from a new source
        // port). Identity is the certificate: this must be a replacement, not
        // a second device, and the address must not flip.
        let second = plan_accept(&mut sessions, "fingerprint", v6, 2);
        assert_eq!(
            second,
            AcceptPlan {
                logical_addr: v4,
                replacement: true,
            }
        );
        assert!(
            !session_is_current(&sessions, "fingerprint", 1),
            "the stale transport must not be able to remove or report the new one"
        );
        assert!(session_is_current(&sessions, "fingerprint", 2));
    }

    #[test]
    fn reconnecting_from_the_same_family_replaces_the_old_transport() {
        let mut sessions = HashMap::new();
        let first = addr("192.168.1.5", 1000);
        let second = addr("192.168.1.5", 4000);
        let _ = plan_accept(&mut sessions, "fingerprint", first, 1);
        let plan = plan_accept(&mut sessions, "fingerprint", second, 2);
        assert_eq!(
            plan.logical_addr, first,
            "the service-facing address stays stable across reconnects"
        );
        assert!(plan.replacement);
    }

    #[test]
    fn different_fingerprints_stay_separate_devices() {
        let mut sessions = HashMap::new();
        let a = addr("192.168.1.5", 1000);
        let b = addr("192.168.1.6", 1000);
        let first = plan_accept(&mut sessions, "fp-a", a, 1);
        let second = plan_accept(&mut sessions, "fp-b", b, 2);
        assert!(!first.replacement);
        assert!(!second.replacement);
        assert_eq!(second.logical_addr, b);
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn unknown_fingerprint_is_never_current() {
        let sessions = HashMap::new();
        assert!(!session_is_current(&sessions, "missing", 1));
    }
}
