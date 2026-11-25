//! The incoming side over real DTLS on loopback: who gets in, what reaches
//! the service, and what a broken or hostile peer can do to a running
//! listener.

use std::{net::Ipv4Addr, time::Duration};

use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
use lan_mouse_ipc::{Capabilities, DirectionPolicy};
use lan_mouse_proto::ProtoEvent;
use tokio::{net::UdpSocket, task::LocalSet};
use webrtc_util::Conn;

use super::{HANDSHAKES_PER_IP, ListenEvent, PairingWindow};
use crate::net_tests::{
    ListenerSetup, cert, closed_within, dial, fingerprint, free_port, loopback, send, send_raw,
};

const HANDSHAKE: Duration = Duration::from_secs(3);
const QUIET: Duration = Duration::from_millis(600);

fn run<F: std::future::Future<Output = ()>>(test: F) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(LocalSet::new().run_until(test));
}

fn ping_received(event: &ListenEvent) -> Option<()> {
    matches!(
        event,
        ListenEvent::Msg {
            event: ProtoEvent::Ping,
            ..
        }
    )
    .then_some(())
}

fn rejections(events: &[ListenEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, ListenEvent::Rejected { .. }))
        .count()
}

/// An authorized device gets in and its events reach the service.
#[test]
fn an_authorized_device_connects_and_is_heard() {
    run(async {
        let peer = cert("auth-peer");
        let mut l = ListenerSetup::default()
            .authorize(&peer)
            .start(cert("auth-listener"))
            .await;
        let conn = dial(l.port, peer.clone(), HANDSHAKE)
            .await
            .expect("handshake");
        let accepted = l
            .expect("accept", HANDSHAKE, |event| match event {
                ListenEvent::Accept { fingerprint, .. } => Some(fingerprint.clone()),
                _ => None,
            })
            .await;
        assert_eq!(accepted, fingerprint(&peer));
        send(&conn, ProtoEvent::Ping).await;
        l.expect("ping", HANDSHAKE, ping_received).await;
        l.listener.terminate().await;
    });
}

/// An unknown device is refused and reported once as a request to approve.
#[test]
fn an_unknown_device_is_refused_and_reported() {
    run(async {
        let stranger = cert("unknown-peer");
        let mut l = ListenerSetup::default()
            .start(cert("unknown-listener"))
            .await;
        assert!(dial(l.port, stranger.clone(), HANDSHAKE).await.is_err());
        let reported = l
            .expect("rejected", HANDSHAKE, |event| match event {
                ListenEvent::Rejected { fingerprint, .. } => Some(fingerprint.clone()),
                _ => None,
            })
            .await;
        assert_eq!(reported, fingerprint(&stranger));
        l.listener.terminate().await;
    });
}

/// Kiosk mode turns every incoming connection away, authorized ones too,
/// without raising approval requests; turning it off lets them in again.
#[test]
fn kiosk_mode_refuses_everyone_silently_until_turned_off() {
    run(async {
        let peer = cert("kiosk-peer");
        let mut l = ListenerSetup {
            kiosk: true,
            ..Default::default()
        }
        .authorize(&peer)
        .start(cert("kiosk-listener"))
        .await;
        assert!(dial(l.port, peer.clone(), HANDSHAKE).await.is_err());
        assert!(
            dial(l.port, cert("kiosk-stranger"), HANDSHAKE)
                .await
                .is_err()
        );
        assert_eq!(
            rejections(&l.drain(QUIET).await),
            0,
            "kiosk mode raised a request"
        );

        *l.accept_incoming.write().unwrap() = true;
        dial(l.port, peer, HANDSHAKE)
            .await
            .expect("allowed again after kiosk mode");
        l.listener.terminate().await;
    });
}

/// A device the user disconnected keeps retrying: refused every time, and
/// never turned into an approval request (the dialog kept reopening).
#[test]
fn a_disconnected_device_retrying_is_refused_without_requests() {
    run(async {
        let peer = cert("suppressed-peer");
        let mut devices = crate::devices::DeviceStore::default();
        devices.suppress_reconnect(fingerprint(&peer), Ipv4Addr::LOCALHOST.into());
        let mut l = ListenerSetup {
            devices,
            ..Default::default()
        }
        .authorize(&peer)
        .start(cert("suppressed-listener"))
        .await;
        for _ in 0..3 {
            assert!(dial(l.port, peer.clone(), HANDSHAKE).await.is_err());
        }
        assert_eq!(rejections(&l.drain(QUIET).await), 0);

        // allowing it again lets it back in
        l.devices
            .write()
            .unwrap()
            .allow_reconnect(&fingerprint(&peer));
        dial(l.port, peer, HANDSHAKE).await.expect("allowed again");
        l.listener.terminate().await;
    });
}

/// A revoked device stays out even with an open pairing window and an
/// address rule covering it, and raises no request.
#[test]
fn a_revoked_device_is_kept_out_by_every_path() {
    run(async {
        let peer = cert("revoked-peer");
        let mut devices = crate::devices::DeviceStore::default();
        devices.deny(fingerprint(&peer));
        let mut l = ListenerSetup {
            devices,
            pairing: Some(PairingWindow {
                until: std::time::Instant::now() + Duration::from_secs(60),
                incoming: DirectionPolicy::allow_all(),
                outgoing: DirectionPolicy::allow_all(),
            }),
            rules: vec![crate::trusted::AddressRule::legacy(
                crate::trusted::TrustedNet::parse("127.0.0.0/8").unwrap(),
            )],
            ..Default::default()
        }
        .authorize(&peer)
        .start(cert("revoked-listener"))
        .await;
        assert!(dial(l.port, peer, HANDSHAKE).await.is_err());
        assert_eq!(rejections(&l.drain(QUIET).await), 0);
        l.listener.terminate().await;
    });
}

/// A pairing window that allows the mouse only holds the adopted device to
/// that from its first packet: no key gets through, motion does.
#[test]
fn a_mouse_only_pairing_cannot_type() {
    run(async {
        let mouse_only = DirectionPolicy {
            allowed: true,
            capabilities: Capabilities::POINTER,
        };
        let mut l = ListenerSetup {
            pairing: Some(PairingWindow {
                until: std::time::Instant::now() + Duration::from_secs(60),
                incoming: mouse_only,
                outgoing: DirectionPolicy::allow_all(),
            }),
            ..Default::default()
        }
        .start(cert("scoped-listener"))
        .await;
        let conn = dial(l.port, cert("scoped-peer"), HANDSHAKE)
            .await
            .expect("pairing accepts the device");
        send(
            &conn,
            ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: 30,
                state: 1,
            })),
        )
        .await;
        send(
            &conn,
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Motion {
                time: 0,
                dx: 2.0,
                dy: 0.0,
            })),
        )
        .await;
        send(&conn, ProtoEvent::Ping).await;
        let events = l.drain(QUIET).await;
        let delivered: Vec<&ProtoEvent> = events
            .iter()
            .filter_map(|event| match event {
                ListenEvent::Msg { event, .. } => Some(event),
                _ => None,
            })
            .collect();
        assert!(
            !delivered
                .iter()
                .any(|e| matches!(e, ProtoEvent::Input(InputEvent::Keyboard(_)))),
            "a key from a mouse-only device reached the service: {delivered:?}"
        );
        assert!(
            delivered
                .iter()
                .any(|e| matches!(e, ProtoEvent::Input(InputEvent::Pointer(_)))),
            "motion was dropped: {delivered:?}"
        );
        l.listener.terminate().await;
    });
}

/// Garbage on the listening port before any handshake (scanners, another
/// program) must not keep real devices out.
#[test]
fn garbage_on_the_port_does_not_block_real_devices() {
    run(async {
        let peer = cert("garbage-peer");
        let mut l = ListenerSetup::default()
            .authorize(&peer)
            .start(cert("garbage-listener"))
            .await;
        let junk = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for size in [0usize, 1, 13, 200, 1400] {
            let bytes: Vec<u8> = (0..size).map(|i| (i * 31 + 7) as u8).collect();
            let _ = junk.send_to(&bytes, loopback(l.port)).await;
        }
        // a DTLS-looking record with a nonsense body
        let _ = junk
            .send_to(
                &[22, 0xfe, 0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 1, 2, 3, 4, 5],
                loopback(l.port),
            )
            .await;
        let conn = dial(l.port, peer, HANDSHAKE)
            .await
            .expect("handshake after garbage");
        send(&conn, ProtoEvent::Ping).await;
        l.expect("ping", HANDSHAKE, ping_received).await;
        l.listener.terminate().await;
    });
}

/// Inside a session, datagrams this build cannot decode (truncated, unknown
/// type, a newer peer's events, broken clipboard frames, a datagram larger
/// than any frame) are skipped; the session keeps working.
#[test]
fn undecodable_datagrams_do_not_end_the_session() {
    run(async {
        let peer = cert("undecodable-peer");
        let mut l = ListenerSetup::default()
            .authorize(&peer)
            .start(cert("undecodable-listener"))
            .await;
        let conn = dial(l.port, peer, HANDSHAKE).await.expect("handshake");
        let junk: Vec<Vec<u8>> = vec![
            vec![0],                  // motion without its fields
            vec![4, 0, 0],            // truncated key
            vec![200, 1, 2, 3],       // unknown event type
            vec![24, 0, 0, 0, 0],     // the next event a newer build might add
            vec![12, 0, 0],           // truncated clipboard offer
            vec![13, 0, 0, 0, 1, 99], // clipboard chunk with an unknown mime
            vec![0x41; 1400],         // a large datagram of nonsense
        ];
        for bytes in &junk {
            send_raw(&conn, bytes).await;
        }
        send(&conn, ProtoEvent::Ping).await;
        l.expect("ping after the junk", HANDSHAKE, ping_received)
            .await;
        assert!(!closed_within(&conn, Duration::from_millis(200)).await);
        l.listener.terminate().await;
    });
}

/// A datagram larger than the listener's receive buffer (a newer peer with
/// bigger frames) must not tear the session down either.
#[test]
fn an_oversized_datagram_does_not_end_the_session() {
    run(async {
        let peer = cert("oversized-peer");
        let mut l = ListenerSetup::default()
            .authorize(&peer)
            .start(cert("oversized-listener"))
            .await;
        let conn = dial(l.port, peer, HANDSHAKE).await.expect("handshake");
        let big = vec![0x42; lan_mouse_proto::clipboard::MAX_CLIPBOARD_FRAME + 200];
        let _ = conn.send(&big).await;
        send(&conn, ProtoEvent::Ping).await;
        l.expect("ping after an oversized datagram", HANDSHAKE, ping_received)
            .await;
        l.listener.terminate().await;
    });
}

/// The same device reconnecting from a new source port replaces its session
/// under the same address: one Accept, and the new transport is heard.
#[test]
fn a_reconnect_replaces_the_session_under_the_same_address() {
    run(async {
        let peer = cert("reconnect-peer");
        let mut l = ListenerSetup::default()
            .authorize(&peer)
            .start(cert("reconnect-listener"))
            .await;
        let first = dial(l.port, peer.clone(), HANDSHAKE).await.expect("first");
        let first_addr = l
            .expect("first accept", HANDSHAKE, |event| match event {
                ListenEvent::Accept { addr, .. } => Some(*addr),
                _ => None,
            })
            .await;
        let second = dial(l.port, peer, HANDSHAKE).await.expect("second");
        send(&second, ProtoEvent::Ping).await;
        let events = l.drain(QUIET).await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ListenEvent::Accept { .. })),
            "a reconnect was announced as a new device"
        );
        let heard_at = events.iter().find_map(|event| match event {
            ListenEvent::Msg {
                event: ProtoEvent::Ping,
                addr,
            } => Some(*addr),
            _ => None,
        });
        assert_eq!(heard_at, Some(first_addr), "the new transport is not heard");
        assert!(
            closed_within(&first, HANDSHAKE).await,
            "the old transport stayed open"
        );
        l.listener.terminate().await;
    });
}

/// Disconnecting a device closes its live transport and reports it once.
#[test]
fn a_local_disconnect_closes_the_transport() {
    run(async {
        let peer = cert("close-peer");
        let mut l = ListenerSetup::default()
            .authorize(&peer)
            .start(cert("close-listener"))
            .await;
        let conn = dial(l.port, peer.clone(), HANDSHAKE)
            .await
            .expect("handshake");
        l.expect("accept", HANDSHAKE, |event| {
            matches!(event, ListenEvent::Accept { .. }).then_some(())
        })
        .await;
        let closed = l.listener.disconnect_fingerprint(&fingerprint(&peer)).await;
        assert_eq!(closed.len(), 1);
        assert!(closed_within(&conn, HANDSHAKE).await);
        let closes = l
            .drain(QUIET)
            .await
            .iter()
            .filter(|e| matches!(e, ListenEvent::Closed { .. }))
            .count();
        assert_eq!(closes, 1, "the close was reported {closes} times");
        l.listener.terminate().await;
    });
}

/// Handshakes per address are capped (a hostile peer cannot make the
/// listener verify certificates without end), and "allow again" grants a
/// fresh budget so the user's own reconnect is not held back by the cap.
#[test]
fn handshakes_are_capped_per_address_and_allow_again_lifts_the_cap() {
    run(async {
        let peer = cert("budget-peer");
        let mut l = ListenerSetup::default()
            .authorize(&peer)
            .start(cert("budget-listener"))
            .await;
        let stranger = cert("budget-stranger");
        for _ in 0..HANDSHAKES_PER_IP {
            let _ = dial(l.port, stranger.clone(), Duration::from_millis(800)).await;
        }
        let _ = l.drain(Duration::from_millis(100)).await;
        assert!(
            dial(l.port, peer.clone(), Duration::from_millis(800))
                .await
                .is_err(),
            "the cap let one more handshake through"
        );
        l.devices
            .write()
            .unwrap()
            .grant_handshake_budget(Ipv4Addr::LOCALHOST.into());
        dial(l.port, peer, HANDSHAKE)
            .await
            .expect("the granted budget lets the device in");
        l.listener.terminate().await;
    });
}

/// Changing the port moves the listener: the new one accepts, the old one
/// stops; a port that is taken is reported and the old one keeps working.
#[test]
fn changing_the_port_moves_the_listener() {
    run(async {
        let peer = cert("port-peer");
        let mut l = ListenerSetup::default()
            .authorize(&peer)
            .start(cert("port-listener"))
            .await;
        let old = l.port;

        let busy = std::net::UdpSocket::bind(("0.0.0.0", free_port())).unwrap();
        let busy_port = busy.local_addr().unwrap().port();
        l.listener.request_port_change(busy_port);
        assert!(
            l.listener.port_changed().await.is_err(),
            "a taken port was accepted"
        );
        dial(old, peer.clone(), HANDSHAKE)
            .await
            .expect("the old port still works after a failed change");
        drop(busy);

        let new = free_port();
        l.listener.request_port_change(new);
        assert_eq!(l.listener.port_changed().await.expect("port change"), new);
        dial(new, peer.clone(), HANDSHAKE)
            .await
            .expect("the new port accepts");
        assert!(
            dial(old, peer, Duration::from_millis(800)).await.is_err(),
            "the old port still accepts"
        );
        l.listener.terminate().await;
    });
}

/// After `terminate` nothing is accepted any more and live peers are closed.
#[test]
fn terminate_closes_everything() {
    run(async {
        let peer = cert("terminate-peer");
        let mut l = ListenerSetup::default()
            .authorize(&peer)
            .start(cert("terminate-listener"))
            .await;
        let conn = dial(l.port, peer.clone(), HANDSHAKE)
            .await
            .expect("handshake");
        l.listener.terminate().await;
        assert!(closed_within(&conn, HANDSHAKE).await);
        assert!(
            dial(l.port, peer, Duration::from_millis(800))
                .await
                .is_err()
        );
    });
}

/// An address rule adopts a device from a matching address only; an
/// address outside it is refused like any unknown device.
#[test]
fn an_address_rule_adopts_only_matching_addresses() {
    run(async {
        let mut covered = ListenerSetup {
            rules: vec![crate::trusted::AddressRule::legacy(
                crate::trusted::TrustedNet::parse("127.0.0.1").unwrap(),
            )],
            ..Default::default()
        }
        .start(cert("rule-listener"))
        .await;
        let newcomer = cert("rule-peer");
        dial(covered.port, newcomer.clone(), HANDSHAKE)
            .await
            .expect("the rule adopts the device");
        let adopted = covered
            .expect("auto-authorized", HANDSHAKE, |event| match event {
                ListenEvent::AutoAuthorized {
                    fingerprint,
                    from_pairing,
                    ..
                } => Some((fingerprint.clone(), *from_pairing)),
                _ => None,
            })
            .await;
        assert_eq!(adopted, (fingerprint(&newcomer), false));
        assert!(covered.authorized.read().unwrap().contains_key(&adopted.0));
        covered.listener.terminate().await;

        let mut elsewhere = ListenerSetup {
            rules: vec![crate::trusted::AddressRule::legacy(
                crate::trusted::TrustedNet::parse("10.9.8.0/24").unwrap(),
            )],
            ..Default::default()
        }
        .start(cert("rule-listener-2"))
        .await;
        assert!(
            dial(elsewhere.port, cert("rule-peer-2"), HANDSHAKE)
                .await
                .is_err()
        );
        elsewhere.listener.terminate().await;
    });
}
