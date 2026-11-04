//! Local network discovery.
//!
//! Three questions, three mechanisms:
//!
//! 1. *Which addresses are alive on my LAN?* — a UDP probe per host. A host
//!    without a listener answers with ICMP port-unreachable, which surfaces as
//!    `ECONNREFUSED`/`WSAECONNRESET` on the connected socket, so even a plain
//!    office printer shows up.
//! 2. *Which of them run this fork?* — the same probe carries a [`MAGIC`]
//!    datagram; a Lan Mouse instance answers with a [`BeaconReply`]. Peers on
//!    an upstream release simply stay silent and are listed as plain hosts.
//! 3. *Wired or wireless?* — resolved from the local interface that routes to
//!    the address. A peer that speaks the beacon reports its own link type,
//!    which then wins over our guess.

use std::{
    collections::HashSet,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket},
    time::Duration,
};

use lan_mouse_ipc::{DiscoveredDevice, LinkKind};
use tokio::{net::UdpSocket, time::timeout};
use tokio_util::sync::CancellationToken;

/// Datagram prefix of the discovery protocol. Keep it short and unlikely to
/// collide with anything else on the port.
pub const MAGIC: &[u8; 8] = b"LMDISC01";

/// UDP port used for discovery. The DTLS listener port must stay untouched:
/// probing it would make every peer show an authorization dialog.
pub const DEFAULT_DISCOVERY_PORT: u16 = 4245;

const PROBE_TIMEOUT: Duration = Duration::from_millis(400);
const MAX_HOSTS_PER_SUBNET: usize = 254;
const CONCURRENCY: usize = 64;

/// Payload a discovering fork sends back.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct BeaconReply {
    port: u16,
    link: LinkKind,
    hostname: String,
    version: String,
}

/// A local IPv4 interface and the network it covers.
#[derive(Debug, Clone)]
pub struct LocalInterface {
    pub name: String,
    pub addr: Ipv4Addr,
    pub prefix: u8,
    pub link: LinkKind,
}

impl LocalInterface {
    /// Every usable host address of the interface, skipping the network and
    /// broadcast addresses and this machine itself.
    fn host_range(&self) -> Vec<Ipv4Addr> {
        let prefix = self.prefix.min(32);
        // Scanning a /16 would take minutes and mostly hit nothing; cap the
        // sweep at the /24 that contains this interface.
        let prefix = prefix.max(24);
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        };
        let local = u32::from(self.addr);
        let network = local & mask;
        let broadcast = network | !mask;
        let start = network.saturating_add(1).max(local.saturating_sub(127));
        let end = broadcast.saturating_sub(1).min(start.saturating_add(253));
        (start..=end)
            .filter(|ip| *ip != local)
            .map(Ipv4Addr::from)
            .take(MAX_HOSTS_PER_SUBNET)
            .collect()
    }
}

/// Link types of the local interfaces, plus the addresses that recently talked
/// to this machine.
pub fn local_interfaces() -> Vec<LocalInterface> {
    let mut interfaces = Vec::new();
    let Ok(addrs) = if_addrs::get_if_addrs() else {
        return interfaces;
    };
    for iface in addrs {
        let if_addrs::IfAddr::V4(v4) = iface.addr else {
            continue;
        };
        if v4.ip.is_loopback() || is_virtual_interface(&iface.name) {
            continue;
        }
        let prefix = netmask_prefix(v4.netmask);
        interfaces.push(LocalInterface {
            name: iface.name.clone(),
            addr: v4.ip,
            prefix,
            link: link_kind_of(&iface.name),
        });
    }
    interfaces
}

/// Where the packet to `ip` would leave this machine.
pub fn link_kind_for_ip(ip: IpAddr) -> LinkKind {
    let IpAddr::V4(ip) = ip else {
        return LinkKind::Unknown;
    };
    for iface in local_interfaces() {
        if iface.prefix == 0 {
            continue;
        }
        let mask = u32::MAX << (32 - iface.prefix.min(32));
        if (u32::from(ip) & mask) == (u32::from(iface.addr) & mask) {
            return iface.link;
        }
    }
    LinkKind::Unknown
}

/// Bridges, tunnels and container networks are not "the local network": every
/// container answers the liveness probe, which would bury the real devices.
fn is_virtual_interface(name: &str) -> bool {
    const VIRTUAL_PREFIXES: &[&str] = &[
        "docker",
        "br-",
        "veth",
        "virbr",
        "vmnet",
        "lxc",
        "podman",
        "cni",
        "flannel",
        "kube",
        "tun",
        "tap",
        "wg",
        "tailscale",
        "zt",
        "ztun",
        "ppp",
        "ip6tnl",
        "sit",
        "dummy",
    ];
    let lower = name.to_lowercase();
    VIRTUAL_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

fn link_kind_of(name: &str) -> LinkKind {
    let lower = name.to_lowercase();
    if lower.starts_with("wl")
        || lower.contains("wi-fi")
        || lower.contains("wifi")
        || lower.contains("wireless")
        || lower.contains("wlan")
    {
        return LinkKind::Wireless;
    }
    #[cfg(target_os = "linux")]
    {
        // The kernel exposes a `wireless` directory for every wireless device.
        if std::path::Path::new(&format!("/sys/class/net/{name}/wireless")).exists() {
            return LinkKind::Wireless;
        }
    }
    if lower.starts_with("en")
        || lower.starts_with("eth")
        || lower.contains("ethernet")
        || lower.starts_with("em")
    {
        return LinkKind::Wired;
    }
    LinkKind::Unknown
}

fn netmask_prefix(netmask: Ipv4Addr) -> u8 {
    u32::from(netmask).count_ones() as u8
}

/// IPv4 addresses this machine recently exchanged packets with.
#[cfg(target_os = "linux")]
fn arp_hosts() -> HashSet<Ipv4Addr> {
    let mut hosts = HashSet::new();
    let Ok(table) = std::fs::read_to_string("/proc/net/arp") else {
        return hosts;
    };
    for line in table.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let Some(ip) = fields.next() else { continue };
        let Ok(ip) = ip.parse::<Ipv4Addr>() else {
            continue;
        };
        if !ip.is_loopback() {
            hosts.insert(ip);
        }
    }
    hosts
}

#[cfg(not(target_os = "linux"))]
fn arp_hosts() -> HashSet<Ipv4Addr> {
    HashSet::new()
}

/// What a single probe learned.
enum HostProbe {
    /// The host answered the beacon: it runs this fork.
    LanMouse(BeaconReply),
    /// The host exists but nothing answers on the discovery port.
    HostOnly,
}

/// Scan every local subnet for hosts and for Lan Mouse instances.
pub async fn scan(discovery_port: u16) -> Vec<DiscoveredDevice> {
    let interfaces = local_interfaces();
    // The broadcast probe is answered by this machine as well; never list
    // ourselves as a remote device.
    let own_addresses: HashSet<Ipv4Addr> = interfaces.iter().map(|i| i.addr).collect();
    let mut results: Vec<DiscoveredDevice> = Vec::new();
    let mut seen: HashSet<Ipv4Addr> = HashSet::new();

    for iface in &interfaces {
        log::debug!(
            "scanning {} ({}/{}) over {}",
            iface.name,
            iface.addr,
            iface.prefix,
            iface.link
        );
        let hosts = iface.host_range();
        for chunk in hosts.chunks(CONCURRENCY) {
            let probes = chunk
                .iter()
                .map(|ip| probe_host(*ip, discovery_port))
                .collect::<Vec<_>>();
            for (ip, outcome) in chunk.iter().zip(futures::future::join_all(probes).await) {
                if own_addresses.contains(ip) {
                    continue;
                }
                let Some(outcome) = outcome else { continue };
                seen.insert(*ip);
                results.push(device_from_probe(*ip, outcome));
            }
        }
    }

    // Broadcast catches hosts whose firewall drops the ICMP error we rely on.
    broadcast_probe(discovery_port, &mut results, &mut seen, &own_addresses).await;

    // Hosts we already exchanged traffic with are worth listing even if the
    // sweep missed them (sleeping laptop, firewall).
    for ip in arp_hosts() {
        if own_addresses.contains(&ip) {
            continue;
        }
        if seen.insert(ip) {
            results.push(DiscoveredDevice {
                ip: IpAddr::V4(ip),
                hostname: None,
                port: None,
                version: None,
                link: link_kind_for_ip(IpAddr::V4(ip)),
                lan_mouse: false,
                configured: false,
            });
        }
    }

    results.sort_by_key(|device| match device.ip {
        IpAddr::V4(ip) => u32::from(ip),
        IpAddr::V6(ip) => ip.segments().iter().map(|s| *s as u32).sum(),
    });
    results
}

fn device_from_probe(ip: Ipv4Addr, outcome: HostProbe) -> DiscoveredDevice {
    match outcome {
        HostProbe::LanMouse(reply) => DiscoveredDevice {
            ip: IpAddr::V4(ip),
            hostname: Some(reply.hostname),
            port: Some(reply.port),
            version: Some(reply.version),
            // The peer knows its own medium; trust it over our guess.
            link: reply.link,
            lan_mouse: true,
            configured: false,
        },
        HostProbe::HostOnly => DiscoveredDevice {
            ip: IpAddr::V4(ip),
            hostname: None,
            port: None,
            version: None,
            link: link_kind_for_ip(IpAddr::V4(ip)),
            lan_mouse: false,
            configured: false,
        },
    }
}

async fn probe_host(ip: Ipv4Addr, discovery_port: u16) -> Option<HostProbe> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.ok()?;
    sock.connect((ip, discovery_port)).await.ok()?;
    sock.send(MAGIC).await.ok()?;

    let mut buf = [0u8; 1024];
    match timeout(PROBE_TIMEOUT, sock.recv(&mut buf)).await {
        Ok(Ok(n)) => parse_reply(&buf[..n]),
        // ICMP port unreachable: the host is up, no Lan Mouse there.
        Ok(Err(e)) if is_unreachable(&e) => Some(HostProbe::HostOnly),
        _ => None,
    }
}

async fn broadcast_probe(
    discovery_port: u16,
    results: &mut Vec<DiscoveredDevice>,
    seen: &mut HashSet<Ipv4Addr>,
    own_addresses: &HashSet<Ipv4Addr>,
) {
    let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await else {
        return;
    };
    if sock.set_broadcast(true).is_err() {
        return;
    }
    let mut targets = vec![Ipv4Addr::BROADCAST];
    for iface in local_interfaces() {
        let prefix = iface.prefix.clamp(24, 32);
        let mask = u32::MAX << (32 - prefix);
        let broadcast = Ipv4Addr::from((u32::from(iface.addr) & mask) | !mask);
        targets.push(broadcast);
    }
    targets.sort();
    targets.dedup();

    for target in targets {
        if sock.send_to(MAGIC, (target, discovery_port)).await.is_err() {
            continue;
        }
    }

    let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
    let mut buf = [0u8; 1024];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                let SocketAddr::V4(from) = from else { continue };
                let ip = *from.ip();
                if own_addresses.contains(&ip) {
                    continue;
                }
                seen.insert(ip);
                if let Some(device) = parse_reply(&buf[..n]).map(|o| device_from_probe(ip, o)) {
                    results.retain(|existing| existing.ip != IpAddr::V4(ip));
                    results.push(device);
                }
            }
            _ => break,
        }
    }
}

fn parse_reply(buf: &[u8]) -> Option<HostProbe> {
    let payload = buf.strip_prefix(MAGIC.as_slice())?;
    let reply: BeaconReply = serde_json::from_slice(payload).ok()?;
    Some(HostProbe::LanMouse(reply))
}

fn is_unreachable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
    )
}

/// Answer discovery probes so other machines can find this one.
///
/// Runs for the whole lifetime of the service. A bind failure (another
/// instance, a sandbox) is not fatal: the machine is simply not discoverable.
pub async fn beacon_task(port: u16, discovery_port: u16, cancel: CancellationToken) {
    let sock = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, discovery_port)).await {
        Ok(sock) => sock,
        Err(e) => {
            log::warn!("network discovery disabled (bind {discovery_port}/udp failed: {e})");
            return;
        }
    };
    log::info!("network discovery listening on udp/{discovery_port}");

    let reply = BeaconReply {
        port,
        link: primary_link_kind(),
        hostname: hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".to_string()),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let payload = match serde_json::to_vec(&reply) {
        Ok(json) => {
            let mut payload = MAGIC.to_vec();
            payload.extend_from_slice(&json);
            payload
        }
        Err(e) => {
            log::warn!("could not encode the discovery reply: {e}");
            return;
        }
    };

    let mut buf = [0u8; 256];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            received = sock.recv_from(&mut buf) => {
                let Ok((n, from)) = received else { continue };
                if !buf[..n].starts_with(MAGIC.as_slice()) {
                    continue;
                }
                let reply = with_link_for(&payload, from.ip());
                if let Err(e) = sock.send_to(&reply, from).await {
                    log::debug!("could not answer discovery probe from {from}: {e}");
                }
            }
        }
    }
}

/// Re-encode the reply with the link type of the interface that reaches the
/// prober, so the answer describes *their* path rather than our default route.
fn with_link_for(payload: &[u8], peer: IpAddr) -> Vec<u8> {
    let Some(json) = payload.strip_prefix(MAGIC.as_slice()) else {
        return payload.to_vec();
    };
    let Ok(mut reply) = serde_json::from_slice::<BeaconReply>(json) else {
        return payload.to_vec();
    };
    let link = link_kind_for_ip(peer);
    if link != LinkKind::Unknown {
        reply.link = link;
    }
    match serde_json::to_vec(&reply) {
        Ok(json) => {
            let mut out = MAGIC.to_vec();
            out.extend_from_slice(&json);
            out
        }
        Err(_) => payload.to_vec(),
    }
}

fn primary_link_kind() -> LinkKind {
    local_interfaces()
        .into_iter()
        .map(|iface| iface.link)
        .find(|link| *link != LinkKind::Unknown)
        .unwrap_or(LinkKind::Unknown)
}

/// Explicitly test whether a host speaks Lan Mouse.
///
/// This performs a real DTLS handshake, so a fork peer may show an
/// authorization dialog — it is only ever run on user request.
pub async fn probe_lan_mouse(
    addr: SocketAddr,
    cert: webrtc_dtls::crypto::Certificate,
    wait: Duration,
) -> bool {
    use webrtc_dtls::{
        config::{Config, ExtendedMasterSecretType},
        conn::DTLSConn,
    };

    let bind = if addr.is_ipv4() {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
    };
    let Ok(udp) = StdUdpSocket::bind(bind) else {
        return false;
    };
    let _ = udp.set_nonblocking(true);
    let Ok(udp_tokio) = UdpSocket::from_std(udp) else {
        return false;
    };
    if udp_tokio.connect(addr).await.is_err() {
        return false;
    }
    let config = Config {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    // Any DTLS answer (including an authentication alert) proves that a
    // Lan Mouse listener owns the port; silence means nothing is there.
    match timeout(
        wait,
        DTLSConn::new(std::sync::Arc::new(udp_tokio), config, true, None),
    )
    .await
    {
        Ok(Ok(_)) => true,
        Ok(Err(_)) => true,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netmask_prefix_is_counted() {
        assert_eq!(netmask_prefix(Ipv4Addr::new(255, 255, 255, 0)), 24);
        assert_eq!(netmask_prefix(Ipv4Addr::new(255, 255, 0, 0)), 16);
        assert_eq!(netmask_prefix(Ipv4Addr::new(255, 255, 255, 255)), 32);
    }

    #[test]
    fn host_range_skips_network_broadcast_and_self() {
        let iface = LocalInterface {
            name: "test0".into(),
            addr: Ipv4Addr::new(192, 168, 1, 10),
            prefix: 24,
            link: LinkKind::Wired,
        };
        let hosts = iface.host_range();
        assert!(hosts.contains(&Ipv4Addr::new(192, 168, 1, 1)));
        assert!(hosts.contains(&Ipv4Addr::new(192, 168, 1, 254)));
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 1, 0)));
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 1, 255)));
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 1, 10)));
    }

    #[test]
    fn wide_subnets_are_capped_to_a_slash_24() {
        let iface = LocalInterface {
            name: "test0".into(),
            addr: Ipv4Addr::new(10, 1, 2, 3),
            prefix: 8,
            link: LinkKind::Wired,
        };
        let hosts = iface.host_range();
        assert!(hosts.len() <= MAX_HOSTS_PER_SUBNET);
        assert!(hosts.iter().all(|ip| ip.octets()[..3] == [10, 1, 2]));
    }

    #[test]
    fn container_and_tunnel_interfaces_are_skipped() {
        assert!(is_virtual_interface("docker0"));
        assert!(is_virtual_interface("br-01464aea7f67"));
        assert!(is_virtual_interface("veth3f2a"));
        assert!(is_virtual_interface("tailscale0"));
        assert!(!is_virtual_interface("wlp128s20f3"));
        assert!(!is_virtual_interface("enp130s0"));
    }

    #[test]
    fn link_kind_recognizes_common_names() {
        assert_eq!(link_kind_of("wlp2s0"), LinkKind::Wireless);
        assert_eq!(link_kind_of("Wi-Fi"), LinkKind::Wireless);
        assert_eq!(link_kind_of("enp3s0"), LinkKind::Wired);
        assert_eq!(link_kind_of("Ethernet"), LinkKind::Wired);
    }

    #[test]
    fn replies_round_trip() {
        let reply = BeaconReply {
            port: 4242,
            link: LinkKind::Wireless,
            hostname: "predator".into(),
            version: "0.11.0".into(),
        };
        let mut payload = MAGIC.to_vec();
        payload.extend_from_slice(&serde_json::to_vec(&reply).unwrap());
        assert!(matches!(
            parse_reply(&payload),
            Some(HostProbe::LanMouse(BeaconReply { port: 4242, .. }))
        ));
        assert!(parse_reply(b"nope").is_none());
        assert!(parse_reply(MAGIC).is_none());
    }
}
