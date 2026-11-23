//! The Network page flow, driven through the frontend IPC exactly like the GUI.
//!
//! A user scans the network, clicks Add on a discovered PC, approves the
//! request on that PC and then moves the mouse over. Every step goes through
//! the real service: the discovery beacon, the request the GUI sends
//! (`AddClient` with the beacon's port and fingerprint), the pairing request
//! on the other PC and the DTLS connection.

use std::{net::IpAddr, time::Duration};

use lan_mouse_ipc::{
    AsyncFrontendEventReader, AsyncFrontendRequestWriter, DirectionPolicy, DiscoveredDevice,
    FrontendEvent, FrontendRequest, connect_async_at,
};
use tokio::task::LocalSet;

use super::{Desk, Focus};

fn init_log() {
    let _ = env_logger::builder()
        .is_test(true)
        .parse_env(env_logger::Env::new().filter("LAN_MOUSE_LOG_LEVEL"))
        .try_init();
}

/// A and B on one desk, unknown to each other (no configured clients, no
/// pre-trusted address), both answering discovery probes.
async fn two_strangers() -> Desk {
    Desk::builder()
        .pc("A", 1920, 1080)
        .pc("B", 1920, 1080)
        .untrusted()
        .discoverable()
        .start()
        .await
}

async fn frontend(
    desk: &Desk,
    name: &str,
) -> (AsyncFrontendEventReader, AsyncFrontendRequestWriter) {
    connect_async_at(&desk.pc(name).ipc_socket(), Some(Duration::from_secs(5)))
        .await
        .unwrap_or_else(|e| panic!("desk-sim: no IPC to {name}: {e}"))
}

/// What the Network page shows for `name` after a scan.
async fn discover(desk: &Desk, name: &str) -> DiscoveredDevice {
    let port = desk.pc(name).discovery_port.expect("discoverable desk");
    crate::discovery::probe(std::net::Ipv4Addr::LOCALHOST, port)
        .await
        .unwrap_or_else(|| panic!("desk-sim: {name} did not answer the discovery probe"))
}

/// Wait for the next pending connection request on a frontend.
async fn next_pending(events: &mut AsyncFrontendEventReader) -> lan_mouse_ipc::PendingView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = events.next_event(Some(left)).await.expect("ipc");
        match event {
            Some(FrontendEvent::PendingRequests(pending)) if !pending.is_empty() => {
                return pending.into_iter().next().expect("one request");
            }
            Some(_) => continue,
            None => panic!("desk-sim: no connection request arrived"),
        }
    }
}

fn connected(desk: &Desk, name: &str) -> bool {
    desk.pc(name)
        .clients
        .get_client_states()
        .iter()
        .any(|(_, _, state)| state.active && state.active_addr.is_some() && state.alive)
}

/// Scan, Add (with `hostname` as the request's name), approve on B, connect,
/// move the mouse over and back.
async fn add_from_the_network_page_and_use(hostname: Option<&str>) {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = two_strangers().await;
            let (_a_events, mut a_requests) = frontend(&desk, "A").await;
            let (mut b_events, mut b_requests) = frontend(&desk, "B").await;

            // Scan: B answers as a Lan Mouse with its listen port and identity.
            let device = discover(&desk, "B").await;
            assert!(device.lan_mouse, "B must be listed as a Lan Mouse");
            assert_eq!(device.port, Some(desk.pc("B").port));
            let fingerprint = device.fingerprint.clone().expect("beacon fingerprint");

            // Add: the request the Network page sends, with the scanned
            // address. The name must not be what makes it work: a real peer's
            // name usually does not resolve ("could not resolve DESKTOP-...").
            let name = hostname.map(str::to_string);
            a_requests
                .request(FrontendRequest::AddClient {
                    hostname: name.clone(),
                    fix_ips: vec![device.ip],
                    port: device.port.expect("port"),
                    position: lan_mouse_ipc::Position::Left,
                    fingerprint: Some(fingerprint),
                })
                .await
                .expect("add client");

            // A dials B; B shows the request and the user approves it.
            b_requests
                .request(FrontendRequest::ListPending)
                .await
                .expect("list pending");
            let pending = next_pending(&mut b_events).await;
            b_requests
                .request(FrontendRequest::RespondPending {
                    fingerprint: pending.fingerprint,
                    accept: true,
                    name: None,
                    position: Some(lan_mouse_ipc::Position::Right),
                    incoming: Some(DirectionPolicy::allow_all()),
                    outgoing: Some(DirectionPolicy::allow_all()),
                })
                .await
                .expect("approve");

            desk.wait_until(
                "A is connected to the device it added from the Network page",
                Duration::from_secs(10),
                |desk| connected(desk, "A"),
            )
            .await;

            // The new entry is named after the beacon, not "unknown".
            let names: Vec<Option<String>> = desk
                .pc("A")
                .clients
                .clients()
                .into_iter()
                .map(|(config, _)| config.hostname)
                .collect();
            assert_eq!(names, vec![name], "{}", desk.report());
            assert!(
                desk.pc("A")
                    .clients
                    .clients()
                    .iter()
                    .all(|(config, _)| config.fix_ips == vec![IpAddr::from([127, 0, 0, 1])]),
                "the scanned address is kept"
            );

            // B sits at A's left: the mouse goes over.
            desk.move_mouse(-1200.0, 0.0).await;
            desk.wait_focus("B").await;
            desk.move_mouse(1000.0, 0.0).await;
            desk.wait_focus("A").await;
            assert_eq!(desk.focus(), Focus::On("A"));
            assert!(desk.pc("A").capture_begins() >= 1);
        })
        .await;
}

/// Regression: a client added in one step from the Network page had no
/// address to dial (`set_config` did not derive the dialable set from
/// `fix_ips`), so Connect never reached the device until it was re-added by
/// typing its address. The Network page used to send no name at all.
#[tokio::test(flavor = "current_thread")]
async fn a_device_added_from_the_network_page_without_a_name_connects() {
    add_from_the_network_page_and_use(None).await;
}

/// Same flow with a name that never resolves (RFC 6761), like a Windows
/// peer's hostname on a network without local name resolution. Also covers
/// the earlier regression where the beacon's pinned fingerprint made Connect
/// fail with "outgoing control is not allowed for this device".
#[tokio::test(flavor = "current_thread")]
async fn a_device_added_from_the_network_page_with_an_unresolvable_name_connects() {
    add_from_the_network_page_and_use(Some("pc-b.invalid")).await;
}

/// The Network page lists only devices that answered as Lan Mouse with the
/// data Add needs; a scanned PC that is not running Lan Mouse is not offered.
#[tokio::test(flavor = "current_thread")]
async fn the_scan_reports_the_port_and_identity_add_needs() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = two_strangers().await;
            let device = discover(&desk, "A").await;
            assert!(device.lan_mouse);
            assert_eq!(device.port, Some(desk.pc("A").port));
            assert!(device.fingerprint.is_some());
            assert!(
                device
                    .hostname
                    .as_deref()
                    .is_some_and(|name| !name.is_empty())
            );
            assert!(!device.configured, "nothing is configured yet");

            // A port with no beacon: nothing answers as Lan Mouse.
            let silent = super::free_udp_port();
            let nothing = crate::discovery::probe(std::net::Ipv4Addr::LOCALHOST, silent).await;
            assert!(
                nothing.is_none_or(|device| !device.lan_mouse),
                "a host without Lan Mouse must not be offered for Add"
            );
        })
        .await;
}
