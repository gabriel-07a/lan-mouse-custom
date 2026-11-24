//! Demo mode for UI work: `LAN_MOUSE_GPUI_DEMO=1` fills the window with a
//! fixed set of devices instead of connecting to the service, covering every
//! link state and the awkward cases (long names, a pairing code, a version
//! mismatch, a pending request, a network scan). `LAN_MOUSE_GPUI_PAGE=devices
//! |network|settings|add` opens that page (`add`: Devices with the Add device
//! dialog open). Requests go nowhere.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use lan_mouse_ipc::{
    AppVersion, ClientConfig, ClientState, DeviceView, DirectionPolicy, DiscoveredDevice,
    FrontendEvent, HistoryEntry, LinkKind, PeerCompatibility, PendingView, Position, Status,
};

pub(crate) fn enabled() -> bool {
    std::env::var_os("LAN_MOUSE_GPUI_DEMO").is_some()
}

/// The page to open on, from `LAN_MOUSE_GPUI_PAGE`.
pub(crate) fn page() -> Option<&'static str> {
    match std::env::var("LAN_MOUSE_GPUI_PAGE").ok()?.as_str() {
        "devices" => Some("devices"),
        "network" => Some("network"),
        "settings" => Some("settings"),
        // the Devices page with the Add device dialog open
        "add" => Some("add"),
        _ => None,
    }
}

fn ip(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 168, 166, last))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn config(name: &str, last: u8, pos: Position, fingerprint: &str) -> ClientConfig {
    ClientConfig {
        hostname: Some(name.to_string()),
        fix_ips: vec![ip(last)],
        pos,
        fingerprint: Some(fingerprint.to_string()),
        ..Default::default()
    }
}

fn connected(last: u8, rtt: u32) -> ClientState {
    ClientState {
        active: true,
        active_addr: Some(SocketAddr::new(ip(last), 4242)),
        alive: true,
        rtt_ms: Some(rtt),
        jitter_ms: Some(1),
        loss_percent: Some(0),
        last_pong_at: Some(now_ms()),
        peer_app_version: AppVersion::parse(env!("CARGO_PKG_VERSION")),
        compatibility: PeerCompatibility::Compatible,
        peer_caps_known: true,
        ..Default::default()
    }
}

pub(crate) fn events() -> Vec<FrontendEvent> {
    let fp = |n: u8| {
        (0..32)
            .map(|i| format!("{:02X}", n.wrapping_mul(37).wrapping_add(i)))
            .collect::<Vec<_>>()
            .join(":")
    };

    let mut bidirectional = connected(166, 3);
    bidirectional.incoming_addr = Some(SocketAddr::new(ip(166), 51544));

    let mut old_version = connected(20, 41);
    old_version.peer_app_version = AppVersion::parse("0.2.18");
    old_version.compatibility = PeerCompatibility::Warning {
        reason: "peer runs 0.2.18; update both machines for the newest hand-off".into(),
    };

    let waiting = ClientState {
        active: true,
        pairing_code: Some("482913".into()),
        last_error: Some("handshake failed".into()),
        ..Default::default()
    };

    let mut reconnecting = connected(31, 12);
    reconnecting.last_pong_at = Some(now_ms().saturating_sub(17_000));

    let receiving = ClientState {
        incoming_addr: Some(SocketAddr::new(ip(44), 60122)),
        ..Default::default()
    };

    let clients = vec![
        (
            0,
            config("DESKTOP-GABRIEL", 166, Position::Right, &fp(1)),
            bidirectional,
        ),
        (
            1,
            config(
                "notebook-trabalho-com-um-nome-muito-comprido-de-verdade.local",
                20,
                Position::Left,
                &fp(2),
            ),
            old_version,
        ),
        (2, config("sala", 57, Position::Top, &fp(3)), waiting),
        (
            3,
            config("macbook-ana", 31, Position::Bottom, &fp(4)),
            reconnecting,
        ),
        (4, config("tv-box", 44, Position::Left, &fp(5)), receiving),
        (
            5,
            config("raspberry-pi-oficina", 99, Position::Bottom, &fp(6)),
            ClientState::default(),
        ),
    ];

    let devices = clients
        .iter()
        .map(|(_, config, state)| DeviceView {
            fingerprint: config.fingerprint.clone().unwrap_or_default(),
            name: config.hostname.clone().unwrap_or_default(),
            position: config.pos,
            addresses: config.fix_ips.clone(),
            port: 4242,
            incoming: DirectionPolicy::allow_all(),
            outgoing: DirectionPolicy::allow_all(),
            active: state.active,
            connected: state.active_addr.is_some(),
            source: "pairing".into(),
        })
        .collect();

    let pending = vec![PendingView {
        fingerprint: fp(9),
        addr: SocketAddr::new(ip(77), 50311),
        name: Some("estudio-de-gravacao-do-segundo-andar".into()),
        sas: "905117".into(),
        previous_fingerprint: None,
        first_seen: now_ms() / 1000 - 40,
        last_seen: now_ms() / 1000,
        attempts: 3,
    }];

    let scan = vec![
        DiscoveredDevice {
            ip: ip(166),
            hostname: Some("DESKTOP-GABRIEL".into()),
            port: Some(4242),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            link: LinkKind::Wired,
            lan_mouse: true,
            configured: true,
            fingerprint: Some(fp(1)),
            fingerprint_verified: true,
            last_seen: Some(now_ms()),
        },
        DiscoveredDevice {
            ip: ip(88),
            hostname: Some("impressora-hp-laserjet-pro-mfp-m428fdw".into()),
            port: None,
            version: None,
            link: LinkKind::Wireless,
            lan_mouse: false,
            configured: false,
            fingerprint: None,
            fingerprint_verified: false,
            last_seen: Some(now_ms()),
        },
        DiscoveredDevice {
            ip: ip(12),
            hostname: Some("notebook-convidado".into()),
            port: Some(4242),
            version: Some("0.2.20".into()),
            link: LinkKind::Wireless,
            lan_mouse: true,
            configured: false,
            fingerprint: Some(fp(12)),
            fingerprint_verified: false,
            last_seen: Some(now_ms()),
        },
    ];

    let history = vec![HistoryEntry {
        ts: now_ms() / 1000 - 3600,
        event: "pairing.accepted".into(),
        fingerprint: Some(fp(1)),
        name: Some("DESKTOP-GABRIEL".into()),
        addr: Some("192.168.166.166:4242".into()),
        detail: None,
    }];

    vec![
        FrontendEvent::Enumerate(clients),
        FrontendEvent::Devices(devices),
        FrontendEvent::PendingRequests(pending),
        FrontendEvent::NetworkScan(scan),
        FrontendEvent::ConnectionHistory(history),
        FrontendEvent::PublicKeyFingerprint(fp(42)),
        FrontendEvent::PortChanged(4242, None),
        FrontendEvent::CaptureStatus(Status::Enabled),
        FrontendEvent::EmulationStatus(Status::Enabled),
    ]
}
