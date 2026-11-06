//! Local network addresses shown in the "This Computer" section.
//!
//! The GUI is a local frontend, so it can enumerate its own interfaces
//! directly instead of round-tripping through the service. `if-addrs` already
//! supports Linux, Windows and macOS, which keeps the address list identical
//! on every platform lan-mouse runs on.

use std::net::IpAddr;

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
        .filter(|(_, ip)| is_usable(*ip))
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

/// The subtitle of the "This computer" row.
pub(crate) fn subtitle(addresses: &[LocalAddress]) -> String {
    if addresses.is_empty() {
        "No local IP address found — is this computer connected to a network?".to_string()
    } else {
        addresses
            .iter()
            .map(|address| address.ip.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Text copied by the row's copy button: every address, one per line.
pub(crate) fn copy_text(addresses: &[LocalAddress]) -> String {
    addresses
        .iter()
        .map(|address| address.ip.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Tooltip for the row: which interface each address belongs to.
pub(crate) fn tooltip(addresses: &[LocalAddress]) -> String {
    if addresses.is_empty() {
        return "No address found. Check the network connection and press refresh.".to_string();
    }
    addresses
        .iter()
        .map(|address| format!("{} ({})", address.ip, address.interface))
        .collect::<Vec<_>>()
        .join("\n")
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
    fn subtitle_lists_one_address_per_line() {
        let addresses = collect_addresses(vec![
            address("eth0", "192.168.1.10"),
            address("wg0", "10.0.0.5"),
        ]);
        assert_eq!(subtitle(&addresses), "10.0.0.5\n192.168.1.10");
    }

    #[test]
    fn empty_list_has_a_clear_placeholder_and_copy_is_empty() {
        assert!(subtitle(&[]).contains("No local IP address"));
        assert!(copy_text(&[]).is_empty());
        assert!(tooltip(&[]).contains("refresh"));
    }

    #[test]
    fn copy_text_contains_only_addresses() {
        let addresses = collect_addresses(vec![
            address("eth0", "192.168.1.10"),
            address("wg0", "10.0.0.5"),
        ]);
        assert_eq!(copy_text(&addresses), "10.0.0.5\n192.168.1.10");
    }

    #[test]
    fn tooltip_shows_the_interface_for_each_address() {
        let addresses = collect_addresses(vec![address("enp3s0", "192.168.1.10")]);
        assert_eq!(tooltip(&addresses), "192.168.1.10 (enp3s0)");
    }
}
