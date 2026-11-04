use futures::{Stream, StreamExt};
use lan_mouse_proto::{
    MAX_EVENT_SIZE, ProtoEvent,
    clipboard::{ClipboardFrame, MAX_CLIPBOARD_FRAME, is_clipboard_frame},
};
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::CertificateDer;
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4},
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

use crate::{crypto, trusted::TrustedNet};

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
    /// A peer connected from an allowlisted address and its certificate was
    /// adopted without asking the user.
    AutoAuthorized {
        addr: SocketAddr,
        fingerprint: String,
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

impl LanMouseListener {
    pub(crate) async fn new(
        port: u16,
        cert: Certificate,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
        trusted_nets: Arc<RwLock<Vec<TrustedNet>>>,
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
                                let conns = conns_clone.clone();
                                let cfg = cfg.clone();
                                spawn_local(async move {
                                    let _permit = permit;
                                    let recorded: Arc<Mutex<Option<String>>> = Default::default();
                                    let auto_authorized =
                                        Arc::new(std::sync::atomic::AtomicBool::new(false));
                                    let mut cfg = cfg;
                                    cfg.verify_peer_certificate = Some(per_connection_verify(
                                        authorized,
                                        trusted,
                                        addr,
                                        recorded.clone(),
                                        auto_authorized.clone(),
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
                                            if auto_authorized.load(
                                                std::sync::atomic::Ordering::Relaxed,
                                            ) {
                                                let _ = listen_tx.send(
                                                    ListenEvent::AutoAuthorized {
                                                        addr,
                                                        fingerprint: fingerprint.clone(),
                                                    },
                                                );
                                            }
                                            let _ = listen_tx
                                                .send(ListenEvent::Accept { addr, fingerprint });
                                            spawn_local(read_loop(
                                                conns,
                                                addr,
                                                dtls,
                                                listen_tx,
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

/// Build the certificate verifier for a single incoming connection.
///
/// It runs inside the DTLS handshake, which is the only place that knows both
/// the peer certificate *and* the address it came from — the address is what
/// makes an allowlist possible at all.
fn per_connection_verify(
    authorized: Arc<RwLock<HashMap<String, String>>>,
    trusted: Arc<RwLock<Vec<TrustedNet>>>,
    addr: SocketAddr,
    recorded: Arc<Mutex<Option<String>>>,
    auto_authorized: Arc<std::sync::atomic::AtomicBool>,
) -> VerifyPeerCertificateFn {
    Arc::new(
        move |certs: &[Vec<u8>], _chains: &[CertificateDer<'static>]| {
            let Some(cert) = certs.first() else {
                return Err(webrtc_dtls::Error::ErrVerifyDataMismatch);
            };
            let fingerprint = crypto::generate_fingerprint(cert);
            *recorded.lock().expect("lock") = Some(fingerprint.clone());

            if authorized.read().expect("lock").contains_key(&fingerprint) {
                return Ok(());
            }

            // An allowlisted address is trusted on first use: the certificate
            // is stored, so later connections are matched by fingerprint.
            let trusted_match = trusted
                .read()
                .expect("lock")
                .iter()
                .any(|net| net.contains(addr.ip()));
            if trusted_match {
                authorized
                    .write()
                    .expect("lock")
                    .insert(fingerprint.clone(), format!("trusted {}", addr.ip()));
                log::info!(
                    "trusted address {}: authorizing device {fingerprint}",
                    addr.ip()
                );
                auto_authorized.store(true, std::sync::atomic::Ordering::Relaxed);
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
    let primary = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port);
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
    conn: ArcConn,
    dtls_tx: Sender<ListenEvent>,
) -> Result<(), Error> {
    let mut b = [0u8; MAX_CLIPBOARD_FRAME];

    loop {
        let n = match conn.recv(&mut b).await {
            Ok(n) => n,
            Err(_) => break,
        };
        if n > 0 && is_clipboard_frame(b[0]) {
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
    Ok(())
}
