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
    collections::{HashMap, HashSet},
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket},
    time::{Duration, Instant},
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

/// Datagram prefix of the unsolicited announcement a peer broadcasts when it
/// starts (and every few seconds while it runs).
pub const ANNOUNCE_MAGIC: &[u8; 8] = b"LMANNC01";

/// How often a running instance announces itself.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(10);

/// Minimum time between two beacon replies to the same source address.
///
/// The beacon is unauthenticated and answers an 8-byte probe with a
/// ~200-byte payload, so without a limit it is a UDP reflector. Replies are
/// also needed for discovery bursts (unicast sweep followed by a broadcast
/// probe), which are seconds apart, so 100 ms is not a functional limit.
const BEACON_REPLY_INTERVAL: Duration = Duration::from_millis(100);

/// Minimum time between two unsolicited announcements accepted from one
/// source address.
const ANNOUNCE_PER_SOURCE_INTERVAL: Duration = Duration::from_secs(1);

/// Maximum number of announcements forwarded to the service per second.
///
/// Bounds the work and memory a spoofed-announcement flood can cause in the
/// service (every forwarded announcement triggers a device-table rebuild).
const ANNOUNCE_BUDGET_PER_SECOND: u32 = 20;

/// Maximum number of source addresses tracked by a [`SourceRateLimiter`].
const RATE_LIMIT_ENTRIES: usize = 256;

/// Per-source rate limiter with a bounded map.
///
/// Used to keep the unauthenticated discovery port from being turned into a
/// reflector/amplifier or a way to flood the service with fake devices.
#[derive(Debug)]
struct SourceRateLimiter {
    interval: Duration,
    seen: HashMap<Ipv4Addr, Instant>,
}

impl SourceRateLimiter {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            seen: HashMap::new(),
        }
    }

    fn allow(&mut self, source: Ipv4Addr) -> bool {
        self.allow_at(source, Instant::now())
    }

    fn allow_at(&mut self, source: Ipv4Addr, now: Instant) -> bool {
        if let Some(previous) = self.seen.get(&source) {
            if now.duration_since(*previous) < self.interval {
                return false;
            }
        } else if self.seen.len() >= RATE_LIMIT_ENTRIES {
            // Bounded memory: forget expired entries, and if the map is still
            // full fail closed instead of growing without bound.
            let cutoff = now.checked_sub(self.interval).unwrap_or(now);
            self.seen.retain(|_, previous| *previous > cutoff);
            if self.seen.len() >= RATE_LIMIT_ENTRIES {
                return false;
            }
        }
        self.seen.insert(source, now);
        true
    }
}

/// Simple per-second packet budget.
#[derive(Debug)]
struct PerSecondBudget {
    window_start: Instant,
    remaining: u32,
    per_second: u32,
}

impl PerSecondBudget {
    fn new(per_second: u32) -> Self {
        Self {
            window_start: Instant::now(),
            remaining: per_second,
            per_second,
        }
    }

    fn allow(&mut self) -> bool {
        self.allow_at(Instant::now())
    }

    fn allow_at(&mut self, now: Instant) -> bool {
        if now.duration_since(self.window_start) >= Duration::from_secs(1) {
            self.window_start = now;
            self.remaining = self.per_second;
        }
        if self.remaining == 0 {
            false
        } else {
            self.remaining -= 1;
            true
        }
    }
}

/// Payload a discovering fork sends back.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct BeaconReply {
    port: u16,
    link: LinkKind,
    hostname: String,
    version: String,
    /// certificate fingerprint, so the peer can be authorized in advance
    #[serde(default)]
    fingerprint: Option<String>,
}

/// A device that announced itself without being asked.
#[derive(Debug, Clone)]
pub struct Announced {
    pub ip: IpAddr,
    pub device: DiscoveredDevice,
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
        // Sweep the whole (at most /24) subnet, not just a window around the
        // local address: the old formula started at `local - 127`, which hid
        // every host below that on a /24 when the interface address' last
        // octet was >= 128.
        let start = network.saturating_add(1);
        let end = broadcast.saturating_sub(1);
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

    // Test/advanced hook: allow probing explicitly listed hosts (e.g. a
    // loopback alias in CI where the normal subnet sweep cannot reach the
    // second instance). No effect unless the variable is set.
    if let Ok(extra) = std::env::var("LAN_MOUSE_DISCOVERY_EXTRA_HOSTS") {
        for raw in extra.split(',') {
            let Ok(ip) = raw.trim().parse::<Ipv4Addr>() else {
                continue;
            };
            if own_addresses.contains(&ip) || !seen.insert(ip) {
                continue;
            }
            let Some(outcome) = probe_host(ip, discovery_port).await else {
                continue;
            };
            results.push(device_from_probe(ip, outcome));
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
                fingerprint: None,
                fingerprint_verified: false,
                last_seen: None,
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
            fingerprint: reply.fingerprint,
            fingerprint_verified: false,
            last_seen: None,
        },
        HostProbe::HostOnly => DiscoveredDevice {
            ip: IpAddr::V4(ip),
            hostname: None,
            port: None,
            version: None,
            link: link_kind_for_ip(IpAddr::V4(ip)),
            lan_mouse: false,
            configured: false,
            fingerprint: None,
            fingerprint_verified: false,
            last_seen: None,
        },
    }
}

async fn probe_host(ip: Ipv4Addr, discovery_port: u16) -> Option<HostProbe> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.ok()?;
    sock.connect((ip, discovery_port)).await.ok()?;
    match sock.send(MAGIC).await {
        Ok(_) => {}
        // Windows surfaces an ICMP port-unreachable as an error on the send
        // itself; a refused port still proves that the host is up.
        Err(e) if is_unreachable(&e) => return Some(HostProbe::HostOnly),
        Err(_) => return None,
    }

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
    for target in broadcast_targets() {
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

/// Parse an unsolicited announcement.
fn parse_announce(buf: &[u8]) -> Option<BeaconReply> {
    let payload = buf.strip_prefix(ANNOUNCE_MAGIC.as_slice())?;
    serde_json::from_slice(payload).ok()
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
pub async fn beacon_task(
    port: u16,
    discovery_port: u16,
    cancel: CancellationToken,
    fingerprint: String,
    announce_tx: local_channel::mpsc::Sender<Announced>,
) {
    let sock = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, discovery_port)).await {
        Ok(sock) => sock,
        Err(e) => {
            log::warn!("network discovery disabled (bind {discovery_port}/udp failed: {e})");
            return;
        }
    };
    let _ = sock.set_broadcast(true);
    log::info!("network discovery listening on udp/{discovery_port}");

    let reply = BeaconReply {
        port,
        link: primary_link_kind(),
        hostname: hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".to_string()),
        version: env!("CARGO_PKG_VERSION").to_string(),
        fingerprint: Some(fingerprint),
    };
    let payload = match encode(MAGIC, &reply) {
        Ok(payload) => payload,
        Err(e) => {
            log::warn!("could not encode the discovery reply: {e}");
            return;
        }
    };
    let announce = match encode(ANNOUNCE_MAGIC, &reply) {
        Ok(payload) => payload,
        Err(e) => {
            log::warn!("could not encode the discovery announcement: {e}");
            return;
        }
    };

    let mut own_addresses: HashSet<Ipv4Addr> =
        local_interfaces().into_iter().map(|i| i.addr).collect();
    let mut announce_interval = tokio::time::interval(ANNOUNCE_INTERVAL);
    // Announce immediately: a peer that is already running should learn about
    // this machine without waiting for the next scan.
    announce_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // The discovery port is unauthenticated: bound how often one source can
    // make us answer (reflection) and how many announcements reach the
    // service (fake-device flood), see docs/qa/protocol.md DISC-02.
    let mut reply_limiter = SourceRateLimiter::new(BEACON_REPLY_INTERVAL);
    let mut announce_limiter = SourceRateLimiter::new(ANNOUNCE_PER_SOURCE_INTERVAL);
    let mut announce_budget = PerSecondBudget::new(ANNOUNCE_BUDGET_PER_SECOND);

    let mut buf = [0u8; 1024];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = announce_interval.tick() => {
                // DHCP/roaming can change our addresses at runtime; refresh
                // the self-detection set so our own announcements are never
                // listed as a remote device.
                own_addresses = local_interfaces().into_iter().map(|i| i.addr).collect();
                for target in broadcast_targets() {
                    if let Err(e) = sock.send_to(&announce, (target, discovery_port)).await {
                        log::debug!("could not announce to {target}: {e}");
                    }
                }
            }
            received = sock.recv_from(&mut buf) => {
                let (n, from) = match received {
                    Ok(pair) => pair,
                    Err(e) => {
                        // A persistent socket error (on Windows an ICMP
                        // port-unreachable surfaces as WSAECONNRESET) must not
                        // turn this loop into a busy spin.
                        log::debug!("discovery receive error: {e}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let packet = &buf[..n];
                if packet.starts_with(MAGIC.as_slice()) {
                    let IpAddr::V4(source) = from.ip() else { continue };
                    if !reply_limiter.allow(source) {
                        log::trace!("rate limiting discovery reply to {source}");
                        continue;
                    }
                    let reply = with_link_for(&payload, from.ip());
                    if let Err(e) = sock.send_to(&reply, from).await {
                        log::debug!("could not answer discovery probe from {from}: {e}");
                    }
                } else if let Some(reply) = parse_announce(packet) {
                    let IpAddr::V4(ip) = from.ip() else { continue };
                    if own_addresses.contains(&ip) {
                        continue;
                    }
                    if !announce_limiter.allow(ip) || !announce_budget.allow() {
                        log::trace!("rate limiting announcement from {ip}");
                        continue;
                    }
                    let device = device_from_probe(ip, HostProbe::LanMouse(reply));
                    let _ = announce_tx.send(Announced {
                        ip: IpAddr::V4(ip),
                        device,
                    });
                }
            }
        }
    }
}

fn encode(prefix: &[u8; 8], reply: &BeaconReply) -> Result<Vec<u8>, serde_json::Error> {
    let json = serde_json::to_vec(reply)?;
    let mut payload = prefix.to_vec();
    payload.extend_from_slice(&json);
    Ok(payload)
}

/// Broadcast addresses of the local subnets plus the limited broadcast.
fn broadcast_targets() -> Vec<Ipv4Addr> {
    let mut targets = vec![Ipv4Addr::BROADCAST];
    for iface in local_interfaces() {
        let prefix = iface.prefix.clamp(24, 32);
        let mask = u32::MAX << (32 - prefix);
        targets.push(Ipv4Addr::from((u32::from(iface.addr) & mask) | !mask));
    }
    targets.sort();
    targets.dedup();
    targets
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
    fn host_range_covers_the_whole_subnet_when_the_host_is_high_in_it() {
        // Regression: the sweep used to start at `local - 127`, so a host at
        // 192.168.1.200 never probed 192.168.1.1 ..= 192.168.1.72.
        let iface = LocalInterface {
            name: "test0".into(),
            addr: Ipv4Addr::new(192, 168, 1, 200),
            prefix: 24,
            link: LinkKind::Wired,
        };
        let hosts = iface.host_range();
        assert!(hosts.contains(&Ipv4Addr::new(192, 168, 1, 1)));
        assert!(hosts.contains(&Ipv4Addr::new(192, 168, 1, 72)));
        assert!(hosts.contains(&Ipv4Addr::new(192, 168, 1, 254)));
        assert_eq!(
            hosts.len(),
            253,
            "a /24 has 254 host addresses minus this machine"
        );
    }

    #[test]
    fn source_rate_limiter_allows_one_reply_per_interval() {
        let mut limiter = SourceRateLimiter::new(Duration::from_millis(100));
        let source = Ipv4Addr::new(192, 168, 1, 5);
        let now = Instant::now();
        assert!(limiter.allow_at(source, now));
        assert!(!limiter.allow_at(source, now + Duration::from_millis(50)));
        assert!(limiter.allow_at(source, now + Duration::from_millis(100)));
        // Other sources have their own budget.
        assert!(limiter.allow_at(
            Ipv4Addr::new(192, 168, 1, 6),
            now + Duration::from_millis(150)
        ));
    }

    #[test]
    fn source_rate_limiter_memory_is_bounded() {
        let mut limiter = SourceRateLimiter::new(Duration::from_millis(100));
        let now = Instant::now();
        for i in 0..(RATE_LIMIT_ENTRIES as u32 * 2) {
            let source = Ipv4Addr::from(i + 1);
            let _ = limiter.allow_at(source, now);
        }
        assert!(
            limiter.seen.len() <= RATE_LIMIT_ENTRIES,
            "the limiter map must stay bounded, got {}",
            limiter.seen.len()
        );
        // Fail closed while the map is full ...
        assert!(!limiter.allow_at(Ipv4Addr::new(255, 255, 255, 254), now));
        // ... but expired entries are pruned after the interval.
        let later = now + Duration::from_millis(101);
        assert!(limiter.allow_at(Ipv4Addr::new(255, 255, 255, 253), later));
        assert!(limiter.seen.len() <= RATE_LIMIT_ENTRIES);
    }

    #[test]
    fn per_second_budget_resets_each_second() {
        let mut budget = PerSecondBudget::new(2);
        let now = Instant::now();
        assert!(budget.allow_at(now));
        assert!(budget.allow_at(now));
        assert!(!budget.allow_at(now + Duration::from_millis(500)));
        assert!(budget.allow_at(now + Duration::from_secs(1)));
        assert!(budget.allow_at(now + Duration::from_secs(1)));
        assert!(!budget.allow_at(now + Duration::from_secs(1)));
    }

    #[test]
    fn replies_round_trip() {
        let reply = BeaconReply {
            port: 4242,
            link: LinkKind::Wireless,
            hostname: "predator".into(),
            version: "0.11.0".into(),
            fingerprint: Some("aa:bb".into()),
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
