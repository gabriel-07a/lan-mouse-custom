//! The dialing side over real DTLS on loopback: address failover, pinning
//! and what the listener can send back.

use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use lan_mouse_ipc::Position as IpcPosition;
use lan_mouse_proto::{Position, ProtoEvent};
use tokio::task::LocalSet;

use super::{LanMouseConnection, LanMouseConnectionError, connect_first};
use crate::{
    client::ClientManager,
    config::ConfigClient,
    listen::ListenEvent,
    net_tests::{ListenerSetup, cert, fingerprint, free_port, loopback},
};

const WAIT: Duration = Duration::from_secs(5);

fn run<F: std::future::Future<Output = ()>>(test: F) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(LocalSet::new().run_until(test));
}

fn client_for(
    port: u16,
    fingerprint: Option<String>,
) -> (ClientManager, lan_mouse_ipc::ClientHandle) {
    let manager = ClientManager::default();
    let handle = manager.add_with_config(ConfigClient {
        ips: HashSet::from([IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        hostname: None,
        port,
        pos: IpcPosition::Right,
        active: true,
        enter_hook: None,
        leave_hook: None,
        sensitivity: 100,
        fingerprint,
    });
    (manager, handle)
}

/// A dead address in front of a live one costs one timeout, then the live
/// one is used.
#[test]
fn a_dead_address_is_skipped_for_the_next_one() {
    run(async {
        let me = cert("failover-me");
        let mut l = ListenerSetup::default()
            .authorize(&me)
            .start(cert("failover-listener"))
            .await;
        let dead = loopback(free_port());
        let live = loopback(l.port);
        let started = std::time::Instant::now();
        let (_conn, used) = connect_first([dead, live], me, Duration::from_millis(400), None, None)
            .await
            .expect("the live address answers");
        assert_eq!(used, live);
        assert!(started.elapsed() < Duration::from_secs(3));
        l.listener.terminate().await;
    });
}

/// No candidate at all is an error, not a hang.
#[test]
fn no_candidate_is_not_connected() {
    run(async {
        let result = connect_first(
            Vec::<SocketAddr>::new(),
            cert("empty-me"),
            Duration::from_millis(200),
            None,
            None,
        )
        .await;
        assert!(matches!(result, Err(LanMouseConnectionError::NotConnected)));
    });
}

/// A saved client pinned to one certificate does not send anything to a
/// different machine on its address (DHCP reuse, a spoofed host).
#[test]
fn input_never_reaches_an_impostor_on_the_saved_address() {
    run(async {
        let me = cert("impostor-me");
        let real = cert("impostor-real");
        let mut impostor = ListenerSetup::default()
            .authorize(&me)
            .start(cert("impostor"))
            .await;
        let (manager, handle) = client_for(impostor.port, Some(fingerprint(&real)));
        let conn = LanMouseConnection::new(me, manager, 1000);
        let _ = conn
            .send(
                ProtoEvent::Enter {
                    pos: Position::Right,
                    serial: 1,
                },
                handle,
            )
            .await;
        let leaked = impostor
            .drain(Duration::from_millis(1500))
            .await
            .into_iter()
            .any(|event| matches!(event, ListenEvent::Msg { .. }));
        assert!(
            !leaked,
            "an event reached a machine with the wrong certificate"
        );
        impostor.listener.terminate().await;
    });
}

/// What the listener sends back that this build cannot read - a datagram
/// too large for any frame, an unknown event - is skipped by the dialing
/// side too, and the link keeps delivering.
#[test]
fn the_dialing_side_skips_what_it_cannot_read() {
    run(async {
        let me = cert("reply-me");
        let mut l = ListenerSetup::default()
            .authorize(&me)
            .start(cert("reply-listener"))
            .await;
        let (manager, handle) = client_for(l.port, None);
        let mut conn = LanMouseConnection::new(me, manager, 2000);
        conn.send(
            ProtoEvent::Enter {
                pos: Position::Right,
                serial: 3,
            },
            handle,
        )
        .await
        .expect("enter");
        let peer = l
            .expect("enter", WAIT, |event| match event {
                ListenEvent::Msg {
                    event: ProtoEvent::Enter { .. },
                    addr,
                } => Some(*addr),
                _ => None,
            })
            .await;
        l.listener
            .reply_raw(
                peer,
                &vec![0x37; lan_mouse_proto::clipboard::MAX_CLIPBOARD_FRAME + 500],
            )
            .await;
        l.listener.reply_raw(peer, &[201, 0, 1]).await;
        l.listener.reply(peer, ProtoEvent::Ack(3)).await;
        let got = tokio::time::timeout(WAIT, async {
            loop {
                if let (_, ProtoEvent::Ack(serial)) = conn.recv().await {
                    return serial;
                }
            }
        })
        .await
        .expect("the link stopped delivering after an unreadable reply");
        assert_eq!(got, 3);
        l.listener.terminate().await;
    });
}

/// Events sent while the first handshake is still running are not lost:
/// the Enter that started the connection arrives before the motion after it.
#[test]
fn events_keep_their_order_across_the_first_handshake() {
    run(async {
        let me = cert("order-me");
        let mut l = ListenerSetup::default()
            .authorize(&me)
            .start(cert("order-listener"))
            .await;
        let (manager, handle) = client_for(l.port, None);
        let conn = LanMouseConnection::new(me, manager, 2000);
        conn.send(
            ProtoEvent::Enter {
                pos: Position::Right,
                serial: 9,
            },
            handle,
        )
        .await
        .expect("enter");
        for i in 0..5 {
            conn.send(
                ProtoEvent::Input(input_event::Event::Pointer(
                    input_event::PointerEvent::Motion {
                        time: i,
                        dx: 1.0,
                        dy: 0.0,
                    },
                )),
                handle,
            )
            .await
            .expect("motion");
        }
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + WAIT;
        while seen.len() < 6 && tokio::time::Instant::now() < deadline {
            if let Some(ListenEvent::Msg { event, .. }) =
                l.next_event(Duration::from_millis(500)).await
            {
                match event {
                    ProtoEvent::Enter { serial, .. } => seen.push(format!("enter{serial}")),
                    ProtoEvent::Input(input_event::Event::Pointer(
                        input_event::PointerEvent::Motion { time, .. },
                    )) => seen.push(format!("motion{time}")),
                    _ => {}
                }
            }
        }
        assert_eq!(seen.first().map(String::as_str), Some("enter9"), "{seen:?}");
        let times: Vec<&String> = seen.iter().filter(|s| s.starts_with("motion")).collect();
        let mut sorted = times.clone();
        sorted.sort();
        assert_eq!(times, sorted, "motion arrived out of order: {seen:?}");
        l.listener.terminate().await;
    });
}

/// Pointer motion from a peer that has not entered this machine (it
/// overtook its `Enter`, or trails its `Leave`) is not applied: it would move
/// this machine's own pointer across its barriers and start a capture back
/// towards that peer, and the two PCs then drove each other.
#[test]
fn motion_from_a_peer_that_has_not_entered_is_not_applied() {
    run(async {
        let peer = cert("gate-peer");
        let l = ListenerSetup::default()
            .authorize(&peer)
            .start(cert("gate-listener"))
            .await;
        let port = l.port;
        let injected = crate::capture::LastInjected::default();
        let (clipboard_tx, _clipboard_rx) = local_channel::mpsc::channel();
        let _emulation = crate::emulation::Emulation::with_factory(
            Some(input_emulation::Backend::Dummy),
            None,
            l.listener,
            clipboard_tx,
            None,
            injected.clone(),
        );
        let conn = crate::net_tests::dial(port, peer, WAIT)
            .await
            .expect("dial");
        let motion = |dx: f64| {
            ProtoEvent::Input(input_event::Event::Pointer(
                input_event::PointerEvent::Motion {
                    time: 0,
                    dx,
                    dy: 0.0,
                },
            ))
        };
        let settle = || tokio::time::sleep(Duration::from_millis(150));

        crate::net_tests::send(&conn, motion(5.0)).await;
        settle().await;
        assert_eq!(
            injected.get(),
            (0.0, 0.0),
            "motion before the Enter was applied"
        );

        crate::net_tests::send(
            &conn,
            ProtoEvent::EnterAt {
                pos: Position::Left,
                serial: 1,
                along: 0.5,
            },
        )
        .await;
        crate::net_tests::send(&conn, motion(3.0)).await;
        settle().await;
        assert_eq!(
            injected.get(),
            (3.0, 0.0),
            "motion after the Enter was dropped"
        );

        crate::net_tests::send(&conn, ProtoEvent::Leave(0)).await;
        crate::net_tests::send(&conn, motion(9.0)).await;
        settle().await;
        assert_eq!(
            injected.get(),
            (3.0, 0.0),
            "motion after the Leave was applied"
        );
    });
}
