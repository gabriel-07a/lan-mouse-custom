//! QA regression tests for peer identity / address-family handling.
//!
//! Reported bug: one peer (Windows 11/10 + Ubuntu 26) alternated between its
//! IPv4 and IPv6 address in the UI, and some setups showed two connected
//! devices for the same peer. Root cause area: identity is keyed by IP in
//! `ClientManager::get_client` / `find_by_ip` (src/client.rs) while a peer can
//! have A+AAAA addresses (and inbound connections from either family), so the
//! service can create a second, auto-added "reverse" client for the same
//! certificate.
//!
//! These tests pin the address-mapping invariants that the net-identity fix
//! (task-17) must preserve; end-to-end duplicate-row behavior is covered by
//! `scripts/qa-two-instances.sh` (scenario `duplicate`).
//!
//! Ownership: tests only (task-21 QA); production code belongs to task-17.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use lan_mouse::client::ClientManager;
use lan_mouse::config::ConfigClient;
use lan_mouse_ipc::Position;

const PORT: u16 = 4242;

fn ip(s: &str) -> IpAddr {
    s.parse().expect("test ip")
}

fn client(ips: &[IpAddr], active: bool) -> ConfigClient {
    ConfigClient {
        ips: HashSet::from_iter(ips.iter().copied()),
        hostname: None,
        port: PORT,
        pos: Position::Right,
        active,
        enter_hook: None,
        leave_hook: None,
        sensitivity: 100,
        fingerprint: None,
    }
}

fn addr(ip: IpAddr) -> SocketAddr {
    SocketAddr::new(ip, PORT)
}

/// One configured client must answer for both address families when DNS
/// returns A+AAAA for it: an inbound packet from either family has to map to
/// the same client handle (otherwise the service adds a duplicate device).
#[test]
fn one_client_maps_ipv4_and_ipv6_to_the_same_handle() {
    let manager = ClientManager::default();
    let v4 = ip("192.0.2.10");
    let v6 = ip("2001:db8::10");
    let handle = manager.add_with_config(client(&[v4], true));

    // DNS resolved both records for the configured hostname.
    manager.set_dns_ips(handle, vec![v4, v6]);

    assert_eq!(manager.get_client(addr(v4)), Some(handle));
    assert_eq!(
        manager.get_client(addr(v6)),
        Some(handle),
        "an inbound IPv6 packet from the same peer must not create a second client"
    );
    assert_eq!(
        manager.clients().len(),
        1,
        "resolving A+AAAA must not duplicate the configured client"
    );
}

/// Repeated DNS updates (resolver refreshes, reconnects, A/AAAA order changes)
/// must not add, remove or renumber clients.
#[test]
fn dns_answer_churn_keeps_one_client_and_one_handle() {
    let manager = ClientManager::default();
    let v4 = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7));
    let v6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 7));
    let handle = manager.add_with_config(client(&[], true));

    for round in 0..50 {
        // Alternate the order in which the resolver reports the answers.
        let answers = if round % 2 == 0 {
            vec![v4, v6]
        } else {
            vec![v6, v4]
        };
        manager.set_dns_ips(handle, answers);
        assert_eq!(manager.get_client(addr(v4)), Some(handle));
        assert_eq!(manager.get_client(addr(v6)), Some(handle));
        assert_eq!(manager.clients().len(), 1, "round {round}");
    }
}

/// Fixed IPs from the config and resolver answers are unioned, and updating
/// either list must not create a new client.
#[test]
fn fix_ips_and_dns_ips_are_a_union_on_the_same_client() {
    let manager = ClientManager::default();
    let fix_v4 = ip("192.0.2.20");
    let dns_v6 = ip("2001:db8::20");
    let handle = manager.add_with_config(client(&[fix_v4], true));

    manager.set_dns_ips(handle, vec![dns_v6]);
    assert_eq!(manager.get_client(addr(fix_v4)), Some(handle));
    assert_eq!(manager.get_client(addr(dns_v6)), Some(handle));

    // Updating the fixed IPs in the UI must update the mapping in place.
    let new_v4 = ip("192.0.2.21");
    manager.set_fix_ips(handle, vec![new_v4]);
    assert_eq!(manager.get_client(addr(new_v4)), Some(handle));
    assert_eq!(
        manager.get_client(addr(fix_v4)),
        None,
        "stale fixed IP must stop matching after an update"
    );
    assert_eq!(manager.clients().len(), 1);
}

/// An address that does not belong to the client must not match: this is the
/// precondition for the service's auto-adoption path (`add_reverse_client`).
#[test]
fn unknown_address_does_not_match_any_client() {
    let manager = ClientManager::default();
    let handle = manager.add_with_config(client(&[ip("192.0.2.30")], true));

    assert!(manager.get_client(addr(ip("192.0.2.31"))).is_none());
    assert!(manager.get_client(addr(ip("2001:db8::30"))).is_none());
    assert_eq!(manager.clients().len(), 1);
    assert_eq!(manager.get_state(handle).map(|(_, s)| s.active), Some(true));
}

/// Incoming packets only map to *active* clients (the service relies on this
/// when deciding whether a peer is already configured). A disabled client must
/// not be re-activated by inbound traffic alone.
#[test]
fn inactive_client_is_not_matched_by_inbound_address() {
    let manager = ClientManager::default();
    let handle = manager.add_with_config(client(&[ip("192.0.2.40")], false));

    assert!(manager.get_client(addr(ip("192.0.2.40"))).is_none());
    assert_eq!(
        manager.get_state(handle).map(|(_, s)| s.active),
        Some(false)
    );
}
