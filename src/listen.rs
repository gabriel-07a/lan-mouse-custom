use futures::{Stream, StreamExt};
use lan_mouse_proto::{
    MAX_EVENT_SIZE, ProtoEvent,
    clipboard::{ClipboardFrame, MAX_CLIPBOARD_FRAME, is_clipboard_frame},
};
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::CertificateDer;
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    rc::Rc,
    sync::{Arc, Mutex, RwLock},
};
use thiserror::Error;
use tokio::{
    sync::Mutex as AsyncMutex,
    task::{JoinHandle, spawn_local},
};
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

pub(crate) struct LanMouseListener {
    listen_rx: Receiver<ListenEvent>,
    listen_tx: Sender<ListenEvent>,
    listen_task: JoinHandle<()>,
    conns: Rc<AsyncMutex<Vec<(SocketAddr, ArcConn)>>>,
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

        let conns: Rc<AsyncMutex<Vec<(SocketAddr, ArcConn)>>> =
            Rc::new(AsyncMutex::new(Vec::new()));

        let conns_clone = conns.clone();
        let listen_task: JoinHandle<()> = {
            let listen_tx = listen_tx.clone();
            let authorized = authorized_keys.clone();
            let trusted = trusted_nets.clone();
            let pairing = pairing_until.clone();
            let devices = devices.clone();
            spawn_local(async move {
                // Bound the number of half-open handshakes so stray UDP
                // traffic cannot spawn unbounded tasks.
                let handshakes = Arc::new(tokio::sync::Semaphore::new(32));
                loop {
                    tokio::select! {
                        c = listener.accept() => match c {
                            Ok((conn, addr)) => {
                                let addr = normalize_addr(addr);
                                let Ok(permit) = handshakes.clone().try_acquire_owned() else {
                                    log::warn!("too many handshakes in flight, dropping {addr}");
                                    continue;
                                };
                                let listen_tx = listen_tx.clone();
                                let authorized = authorized.clone();
                                let trusted = trusted.clone();
                                let pairing = pairing.clone();
                                let devices = devices.clone();
                                let conns = conns_clone.clone();
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
                                        addr,
                                        recorded.clone(),
                                        auto_source.clone(),
                                    ));
                                    // `false` = server role, as in the
                                    // upstream listener wrapper.
                                    match DTLSConn::new(conn, cfg, false, None).await {
                                        Ok(dtls) => {
                                            let dtls = Arc::new(dtls);
                                            log::info!("dtls client connected, ip: {addr}");
                                            conns.lock().await.push((addr, dtls.clone()));
                                            let fingerprint = dtls
                                                .connection_state()
                                                .await
                                                .peer_certificates
                                                .first()
                                                .map(|cert| crypto::generate_fingerprint(cert))
                                                .unwrap_or_default();
                                            // Auto-authorization (allowlisted
                                            // address or pairing window) is
                                            // only persisted once the
                                            // handshake actually succeeded, so
                                            // a failed/aborted handshake
                                            // cannot leave an orphaned pin.
                                            if let Some(source) =
                                                auto_source.lock().expect("lock").take()
                                            {
                                                let (description, incoming, outgoing) = match source
                                                {
                                                    AutoSource::TrustedAddress {
                                                        incoming,
                                                        outgoing,
                                                    } => (
                                                        format!("trusted {}", addr.ip()),
                                                        incoming,
                                                        outgoing,
                                                    ),
                                                    AutoSource::Pairing {
                                                        incoming,
                                                        outgoing,
                                                    } => {
                                                        // Single use: the
                                                        // window allows exactly
                                                        // one new device.
                                                        let _ = pairing
                                                            .write()
                                                            .expect("lock")
                                                            .take();
                                                        (
                                                            format!("paired {}", addr.ip()),
                                                            incoming,
                                                            outgoing,
                                                        )
                                                    }
                                                };
                                                authorized
                                                    .write()
                                                    .expect("lock")
                                                    .insert(fingerprint.clone(), description);
                                                log::info!(
                                                    "auto-authorized {addr} ({fingerprint})"
                                                );
                                                let _ = listen_tx.send(
                                                    ListenEvent::AutoAuthorized {
                                                        addr,
                                                        fingerprint: fingerprint.clone(),
                                                        from_pairing: matches!(
                                                            source,
                                                            AutoSource::Pairing { .. }
                                                        ),
                                                        incoming,
                                                        outgoing,
                                                    },
                                                );
                                            }
                                            let _ = listen_tx.send(ListenEvent::Accept {
                                                addr,
                                                fingerprint: fingerprint.clone(),
                                            });
                                            spawn_local(read_loop(
                                                conns,
                                                addr,
                                                fingerprint,
                                                dtls,
                                                listen_tx,
                                                devices,
                                                authorized,
                                            ));
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
        let conns = self.conns.lock().await;
        for (_, conn) in conns.iter() {
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
    pub(crate) async fn reply_raw(&self, addr: SocketAddr, bytes: &[u8]) {
        let conns = self.conns.lock().await;
        for (a, conn) in conns.iter() {
            if *a == addr {
                let _ = conn.send(bytes).await;
            }
        }
    }

    /// Close an active incoming connection.
    ///
    /// Used when a device is revoked while it is still connected: removing the
    /// fingerprint alone would leave the live DTLS session able to inject
    /// input until it drops on its own.
    pub(crate) async fn disconnect(&self, addr: SocketAddr) {
        let conn = {
            let mut conns = self.conns.lock().await;
            conns
                .iter()
                .position(|(a, _)| *a == addr)
                .map(|index| conns.remove(index).1)
        };
        if let Some(conn) = conn {
            log::info!("closing incoming connection from {addr}");
            let _ = conn.close().await;
        }
    }

    pub(crate) async fn get_certificate_fingerprint(&self, addr: SocketAddr) -> Option<String> {
        if let Some(conn) = self
            .conns
            .lock()
            .await
            .iter()
            .find(|(a, _)| *a == addr)
            .map(|(_, c)| c.clone())
        {
            let conn: &DTLSConn = conn.as_any().downcast_ref().expect("dtls conn");
            let certs = conn.connection_state().await.peer_certificates;
            let cert = certs.first()?;
            let fingerprint = crypto::generate_fingerprint(cert);
            Some(fingerprint)
        } else {
            None
        }
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
fn per_connection_verify(
    authorized: Arc<RwLock<HashMap<String, String>>>,
    trusted: Arc<RwLock<Vec<AddressRule>>>,
    pairing_until: Arc<RwLock<Option<PairingWindow>>>,
    devices: Arc<RwLock<DeviceStore>>,
    addr: SocketAddr,
    recorded: Arc<Mutex<Option<String>>>,
    auto_source: Arc<Mutex<Option<AutoSource>>>,
) -> VerifyPeerCertificateFn {
    Arc::new(
        move |certs: &[Vec<u8>], _chains: &[CertificateDer<'static>]| {
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

/// Listen on a dual-stack socket on Linux (IPv4-mapped addresses are
/// normalized when accepted), and on IPv4 elsewhere.
async fn bind_listener(port: u16) -> Result<impl Listener, webrtc_util::Error> {
    #[cfg(target_os = "linux")]
    let primary = SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), port);
    #[cfg(not(target_os = "linux"))]
    let primary = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);

    match webrtc_util::conn::conn_udp_listener::listen(primary).await {
        Ok(listener) => Ok(listener),
        Err(e) => {
            #[cfg(target_os = "linux")]
            {
                log::warn!("could not listen on [::]:{port} ({e}); falling back to 0.0.0.0");
                let fallback = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
                webrtc_util::conn::conn_udp_listener::listen(fallback).await
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(e)
            }
        }
    }
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

async fn read_loop(
    conns: Rc<AsyncMutex<Vec<(SocketAddr, ArcConn)>>>,
    addr: SocketAddr,
    fingerprint: String,
    conn: ArcConn,
    dtls_tx: Sender<ListenEvent>,
    devices: Arc<RwLock<DeviceStore>>,
    authorized: Arc<RwLock<HashMap<String, String>>>,
) -> Result<(), Error> {
    let mut b = [0u8; MAX_CLIPBOARD_FRAME];
    let mut blocked = 0u64;

    loop {
        let n = match conn.recv(&mut b).await {
            Ok(n) => n,
            Err(_) => break,
        };
        if n > 0 && is_clipboard_frame(b[0]) {
            if !incoming_policy(&devices, &authorized, &fingerprint).allows(Capabilities::CLIPBOARD)
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
    let mut conns = conns.lock().await;
    if let Some(index) = conns.iter().position(|(a, _)| *a == addr) {
        conns.remove(index);
    }
    let _ = dtls_tx.send(ListenEvent::Closed { addr });
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
