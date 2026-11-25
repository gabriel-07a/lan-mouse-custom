//! Network test suite: the real transports (DTLS over UDP, the discovery
//! beacon) driven over loopback with hand-made peers, for the behaviour unit
//! tests of single functions cannot show - what a hostile, buggy or older
//! peer actually gets from a running listener.
//!
//! The suites for private items live next to their module
//! (`listen/connect/discovery`, included with `#[path]`); this module holds
//! what they share.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU16, Ordering},
    },
    time::Duration,
};

use futures::StreamExt;
use lan_mouse_proto::{MAX_EVENT_SIZE, ProtoEvent};
use tokio::net::UdpSocket;
use webrtc_dtls::{
    config::{Config as DtlsConfig, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::Conn;

use crate::{
    crypto,
    devices::DeviceStore,
    listen::{LanMouseListener, ListenEvent, PairingWindow},
    trusted::AddressRule,
};

pub(crate) fn cert(name: &str) -> Certificate {
    let dir = std::env::temp_dir().join("lan-mouse-net-tests");
    std::fs::create_dir_all(&dir).expect("test dir");
    let path = dir.join(format!("cert-{}-{name}.pem", std::process::id()));
    let _ = std::fs::remove_file(&path);
    crypto::generate_key_and_cert(&path).expect("generate certificate")
}

pub(crate) fn fingerprint(cert: &Certificate) -> String {
    crypto::certificate_fingerprint(cert)
}

static PORTS: AtomicU16 = AtomicU16::new(0);

/// A UDP port on loopback that was free a moment ago (a range of its own, so
/// the suites running in parallel with the older tests do not collide).
pub(crate) fn free_port() -> u16 {
    loop {
        let port = 52000 + PORTS.fetch_add(1, Ordering::Relaxed) % 9000;
        if std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

pub(crate) fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// What the listener is built with; every field defaults to "nothing
/// trusted, incoming allowed".
#[derive(Default)]
pub(crate) struct ListenerSetup {
    pub authorized: HashMap<String, String>,
    pub rules: Vec<AddressRule>,
    pub pairing: Option<PairingWindow>,
    pub devices: DeviceStore,
    pub kiosk: bool,
}

pub(crate) struct TestListener {
    pub listener: LanMouseListener,
    pub port: u16,
    pub devices: Arc<RwLock<DeviceStore>>,
    pub accept_incoming: Arc<RwLock<bool>>,
    pub authorized: Arc<RwLock<HashMap<String, String>>>,
}

impl ListenerSetup {
    pub(crate) fn authorize(mut self, cert: &Certificate) -> Self {
        self.authorized.insert(fingerprint(cert), "peer".into());
        self
    }

    pub(crate) async fn start(self, cert: Certificate) -> TestListener {
        let port = free_port();
        let devices = Arc::new(RwLock::new(self.devices));
        let accept_incoming = Arc::new(RwLock::new(!self.kiosk));
        let authorized = Arc::new(RwLock::new(self.authorized));
        let listener = LanMouseListener::new(
            port,
            cert,
            authorized.clone(),
            Arc::new(RwLock::new(self.rules)),
            Arc::new(RwLock::new(self.pairing)),
            devices.clone(),
            accept_incoming.clone(),
        )
        .await
        .expect("listener");
        TestListener {
            listener,
            port,
            devices,
            accept_incoming,
            authorized,
        }
    }
}

impl TestListener {
    /// The next listener event, or `None` when nothing arrives within `wait`.
    pub(crate) async fn next_event(&mut self, wait: Duration) -> Option<ListenEvent> {
        tokio::time::timeout(wait, self.listener.next())
            .await
            .ok()
            .flatten()
    }

    /// Skip events until `want` accepts one; panics after `wait`.
    pub(crate) async fn expect<T>(
        &mut self,
        what: &str,
        wait: Duration,
        mut want: impl FnMut(&ListenEvent) -> Option<T>,
    ) -> T {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self.next_event(left).await {
                Some(event) => {
                    if let Some(found) = want(&event) {
                        return found;
                    }
                }
                None => panic!("no listener event: {what}"),
            }
        }
    }

    /// Every event within `wait`.
    pub(crate) async fn drain(&mut self, wait: Duration) -> Vec<ListenEvent> {
        let deadline = tokio::time::Instant::now() + wait;
        let mut events = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return events;
            }
            match self.next_event(left).await {
                Some(event) => events.push(event),
                None => return events,
            }
        }
    }
}

/// A raw DTLS client: the handshake result, so refusals can be asserted.
pub(crate) async fn dial(
    port: u16,
    cert: Certificate,
    wait: Duration,
) -> Result<Arc<DTLSConn>, String> {
    let udp = Arc::new(
        UdpSocket::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?,
    );
    udp.connect(loopback(port))
        .await
        .map_err(|e| e.to_string())?;
    let config = DtlsConfig {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    match tokio::time::timeout(wait, DTLSConn::new(udp, config, true, None)).await {
        Ok(Ok(conn)) => Ok(Arc::new(conn)),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("handshake timed out".into()),
    }
}

pub(crate) async fn send(conn: &DTLSConn, event: ProtoEvent) {
    let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
    conn.send(&buf[..len]).await.expect("send");
}

pub(crate) async fn send_raw(conn: &DTLSConn, bytes: &[u8]) {
    conn.send(bytes).await.expect("send raw");
}

/// Whether the transport was closed by the other side within `wait`.
pub(crate) async fn closed_within(conn: &DTLSConn, wait: Duration) -> bool {
    let mut buf = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, conn.recv(&mut buf)).await {
            Err(_) => return false,
            Ok(Err(_)) => return true,
            Ok(Ok(_)) => continue,
        }
    }
}
