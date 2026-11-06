//! Local network addresses shown in the "This Computer" section.
//!
//! The GUI is a local frontend, so it can enumerate its own interfaces
//! directly instead of round-tripping through the service. `if-addrs` already
//! supports Linux, Windows and macOS, which keeps the address list identical
//! on every platform lan-mouse runs on.

use std::net::{IpAddr, SocketAddr, UdpSocket};

/// One address of this machine and the interface it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalAddress {
    pub ip: IpAddr,
    pub interface: String,
}

/// Enumerate the addresses other computers on the network can use to reach
/// this one.
///
/// Loopback, unspecified, multicast and link-local addresses are skipped:
/// they are either not reachable from another machine or need a scope ID that
/// cannot be typed by hand. IPv4 addresses come first so the usual LAN address
/// is the first line shown to the user.
pub(crate) fn local_addresses() -> Vec<LocalAddress> {
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        log::warn!("could not enumerate the local network interfaces");
        return Vec::new();
    };
    collect_addresses(interfaces.into_iter().map(|interface| {
        let ip = match interface.addr {
            if_addrs::IfAddr::V4(v4) => IpAddr::V4(v4.ip),
            if_addrs::IfAddr::V6(v6) => IpAddr::V6(v6.ip),
        };
        (interface.name, ip)
    }))
}

/// Build the sorted, de-duplicated address list from `(interface, ip)` pairs.
///
/// Split out from [`local_addresses`] so the filtering and ordering can be
/// unit-tested without depending on the host's network setup.
fn collect_addresses<I>(interfaces: I) -> Vec<LocalAddress>
where
    I: IntoIterator<Item = (String, IpAddr)>,
{
    let mut addresses = interfaces
        .into_iter()
        .filter(|(interface, ip)| is_usable(*ip) && !is_container_interface(interface))
        .map(|(interface, ip)| LocalAddress { ip, interface })
        .collect::<Vec<_>>();

    // A machine can report the same IP for two interfaces (bridges, bonds,
    // VPN routing). Keep the first one so the list does not repeat itself.
    addresses.sort_by_key(|address| (address.ip, address.interface.clone()));
    addresses.dedup_by_key(|address| address.ip);
    addresses
}

/// Whether another machine can reasonably use this address to reach us.
fn is_usable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_loopback() && !v4.is_unspecified() && !v4.is_link_local() && !v4.is_multicast()
        }
        IpAddr::V6(v6) => {
            !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_multicast()
                // Link-local IPv6 needs a scope ID (`fe80::1%eth0`), which a
                // peer cannot type into the other PC's address field.
                && !(v6.segments()[0] & 0xffc0 == 0xfe80)
        }
    }
}

/// Whether an interface belongs to a container/VM bridge that is not
/// reachable from another computer on the LAN (Docker, libvirt, Multipass,
/// podman, VMs). Those addresses made the old multi-address row confusing.
fn is_container_interface(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "docker0"
        || name.starts_with("docker")
        || name.starts_with("br-")
        || name.starts_with("veth")
        || name.starts_with("virbr")
        || name.starts_with("vmnet")
        || name.starts_with("lxc")
        || name.starts_with("cni")
        || name.starts_with("flannel")
        || name.starts_with("podman")
}

/// The single address the "This computer" row should show.
///
/// The default route is the best answer: it is the address the kernel would
/// use to reach the rest of the network, so it is the one the other computer
/// needs. Probing it with a UDP `connect` does a route lookup without sending
/// a packet and works on Linux, Windows and macOS. When there is no default
/// route (offline LAN, isolated switch), fall back to ranking the enumerated
/// addresses: real interfaces before containers, IPv4 before IPv6, private
/// before public.
pub(crate) fn primary_address(addresses: &[LocalAddress]) -> Option<LocalAddress> {
    choose_primary(addresses, default_route_ip())
}

fn default_route_ip() -> Option<IpAddr> {
    route_probe("8.8.8.8:80".parse().ok()?)
        .or_else(|| route_probe("[2001:4860:4860::8888]:80".parse().ok()?))
}

fn route_probe(destination: SocketAddr) -> Option<IpAddr> {
    let bind = if destination.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect(destination).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// Pure selection step, split from the socket probe so it can be tested.
fn choose_primary(addresses: &[LocalAddress], route_ip: Option<IpAddr>) -> Option<LocalAddress> {
    if let Some(route_ip) = route_ip {
        if let Some(address) = addresses.iter().find(|address| address.ip == route_ip) {
            return Some(address.clone());
        }
        // The route may point at an interface the enumerator hid (rare); the
        // address is still the correct one to show.
        return Some(LocalAddress {
            ip: route_ip,
            interface: "default route".to_string(),
        });
    }
    addresses
        .iter()
        .min_by_key(|address| {
            (
                u8::from(is_container_interface(&address.interface)),
                u8::from(address.ip.is_ipv6()),
                u8::from(!is_private(address.ip)),
                address.ip,
            )
        })
        .cloned()
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private(),
        IpAddr::V6(v6) => v6.segments()[0] & 0xfe00 == 0xfc00,
    }
}

/// The subtitle of the "This computer" row.
pub(crate) fn subtitle(address: Option<&LocalAddress>) -> String {
    address.map_or_else(
        || "No local IP address found — is this computer connected to a network?".to_string(),
        |address| address.ip.to_string(),
    )
}

/// Text copied by the row's copy button: every address, one per line.
pub(crate) fn copy_text(address: Option<&LocalAddress>) -> String {
    address.map_or_else(String::new, |address| address.ip.to_string())
}

/// Tooltip for the row: which interface each address belongs to.
pub(crate) fn tooltip(primary: Option<&LocalAddress>, all: &[LocalAddress]) -> String {
    let Some(primary) = primary else {
        return "No address found. Check the network connection and press refresh.".to_string();
    };
    let mut text = format!("{} ({})", primary.ip, primary.interface);
    let others = all
        .iter()
        .filter(|address| address.ip != primary.ip)
        .map(|address| format!("{} ({})", address.ip, address.interface))
        .collect::<Vec<_>>();
    if !others.is_empty() {
        text.push_str("\nOther local addresses:\n");
        text.push_str(&others.join("\n"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().expect("test address")
    }

    fn address(interface: &str, value: &str) -> (String, IpAddr) {
        (interface.to_string(), ip(value))
    }

    fn local(value: &str) -> LocalAddress {
        LocalAddress {
            ip: ip(value),
            interface: "eth0".to_string(),
        }
    }

    #[test]
    fn skips_loopback_link_local_multicast_and_unspecified_addresses() {
        let collected = collect_addresses(vec![
            address("lo", "127.0.0.1"),
            address("eth0", "192.168.1.10"),
            address("eth0", "169.254.10.20"),
            address("eth0", "224.0.0.1"),
            address("eth0", "::1"),
            address("eth0", "fe80::1"),
            address("eth0", "2001:db8::5"),
            address("eth0", "0.0.0.0"),
        ]);
        let ips = collected
            .iter()
            .map(|address| address.ip)
            .collect::<Vec<_>>();
        assert_eq!(ips, vec![ip("192.168.1.10"), ip("2001:db8::5")]);
    }

    #[test]
    fn container_and_vm_interfaces_are_hidden() {
        let collected = collect_addresses(vec![
            address("docker0", "172.17.0.1"),
            address("br-1234", "172.18.0.1"),
            address("veth12", "172.19.0.1"),
            address("virbr0", "192.168.122.1"),
            address("vmnet1", "172.16.0.1"),
            address("wlp3s0", "192.168.166.197"),
        ]);
        let ips = collected
            .iter()
            .map(|address| address.ip.to_string())
            .collect::<Vec<_>>();
        assert_eq!(ips, vec!["192.168.166.197"]);
    }

    #[test]
    fn ipv4_addresses_come_first_and_are_numerically_sorted() {
        let collected = collect_addresses(vec![
            address("wg0", "10.0.0.5"),
            address("eth0", "192.168.1.10"),
            address("eth0", "9.0.0.1"),
            address("eth0", "2001:db8::2"),
            address("eth0", "2001:db8::1"),
        ]);
        let ips = collected
            .iter()
            .map(|address| address.ip.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            ips,
            vec![
                "9.0.0.1",
                "10.0.0.5",
                "192.168.1.10",
                "2001:db8::1",
                "2001:db8::2"
            ]
        );
    }

    #[test]
    fn duplicate_ips_are_listed_once() {
        let collected = collect_addresses(vec![
            address("eth0", "192.168.1.10"),
            address("docker0", "192.168.1.10"),
        ]);
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].ip, ip("192.168.1.10"));
    }

    #[test]
    fn default_route_address_wins() {
        let addresses = vec![local("172.17.0.1"), local("192.168.166.197")];
        // The route wins even when it is not the first enumerated address.
        let primary = choose_primary(&addresses, Some(ip("192.168.166.197")));
        assert_eq!(primary, Some(local("192.168.166.197")));
    }

    #[test]
    fn route_address_not_in_the_list_is_still_used() {
        let addresses = vec![local("192.168.1.10")];
        let primary = choose_primary(&addresses, Some(ip("10.1.2.3")));
        let primary = primary.expect("route address");
        assert_eq!(primary.ip, ip("10.1.2.3"));
        assert_eq!(primary.interface, "default route");
    }

    #[test]
    fn fallback_prefers_real_ipv4_over_container_ipv6_and_public() {
        let addresses = vec![
            LocalAddress {
                ip: ip("172.17.0.1"),
                interface: "docker0".to_string(),
            },
            LocalAddress {
                ip: ip("2001:db8::5"),
                interface: "eth0".to_string(),
            },
            LocalAddress {
                ip: ip("100.64.0.1"),
                interface: "wlan0".to_string(),
            },
            LocalAddress {
                ip: ip("10.0.0.5"),
                interface: "eth0".to_string(),
            },
        ];
        // No route (isolated LAN): real IPv4 private first, then public IPv4,
        // then real IPv6, then container interfaces.
        let primary = choose_primary(&addresses, None).expect("fallback address");
        assert_eq!(primary.ip, ip("10.0.0.5"));
        assert_eq!(primary.interface, "eth0");
    }

    #[test]
    fn no_address_yields_none() {
        assert_eq!(choose_primary(&[], None), None);
    }

    #[test]
    fn subtitle_and_copy_describe_one_address_only() {
        let address = local("192.168.166.197");
        assert_eq!(subtitle(Some(&address)), "192.168.166.197");
        assert!(!subtitle(Some(&address)).contains('\n'));
        assert_eq!(copy_text(Some(&address)), "192.168.166.197");
        assert!(subtitle(None).contains("No local IP address"));
        assert!(copy_text(None).is_empty());
    }

    #[test]
    fn tooltip_lists_primary_and_other_addresses() {
        let primary = local("192.168.166.197");
        let other = LocalAddress {
            ip: ip("10.0.0.5"),
            interface: "wg0".to_string(),
        };
        let text = tooltip(Some(&primary), &[primary.clone(), other]);
        assert!(text.starts_with("192.168.166.197 (eth0)"));
        assert!(text.contains("Other local addresses:"));
        assert!(text.contains("10.0.0.5 (wg0)"));
        assert!(tooltip(None, &[]).contains("refresh"));
    }
}
