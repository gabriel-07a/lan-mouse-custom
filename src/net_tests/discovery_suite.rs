//! The discovery beacon running on a real UDP socket: what it answers, what
//! it forwards to the service, and what it refuses to do for a stranger.

use std::{
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

use tokio::{net::UdpSocket, task::LocalSet};
use tokio_util::sync::CancellationToken;

use super::{ANNOUNCE_MAGIC, Announced, BeaconReply, MAGIC, beacon_task, encode, probe};
use crate::net_tests::{free_port, loopback};

fn run<F: std::future::Future<Output = ()>>(test: F) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(LocalSet::new().run_until(test));
}

struct Beacon {
    port: u16,
    cancel: CancellationToken,
    announced: local_channel::mpsc::Receiver<Announced>,
    task: tokio::task::JoinHandle<()>,
}

async fn beacon(listen_port: u16) -> Beacon {
    let port = free_port();
    let cancel = CancellationToken::new();
    let (tx, announced) = local_channel::mpsc::channel();
    let task = tokio::task::spawn_local(beacon_task(
        listen_port,
        port,
        cancel.clone(),
        "aa:bb:cc".into(),
        tx,
        false,
    ));
    // let it bind
    tokio::time::sleep(Duration::from_millis(50)).await;
    Beacon {
        port,
        cancel,
        announced,
        task,
    }
}

impl Beacon {
    async fn stop(self) {
        self.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), self.task)
            .await
            .expect("the beacon stops when cancelled")
            .expect("beacon task");
    }

    async fn next_announcement(&mut self, wait: Duration) -> Option<Announced> {
        tokio::time::timeout(wait, self.announced.recv())
            .await
            .ok()
            .flatten()
    }
}

/// Every datagram that arrives at `socket` within `wait`.
async fn replies(socket: &UdpSocket, wait: Duration) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + wait;
    let mut buf = [0u8; 2048];
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, socket.recv(&mut buf)).await {
            Ok(Ok(n)) => out.push(buf[..n].to_vec()),
            _ => return out,
        }
    }
}

async fn prober(port: u16) -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket.connect(loopback(port)).await.unwrap();
    socket
}

/// A probe is answered with the listen port, the version and the
/// certificate fingerprint the Network page needs to add the device.
#[test]
fn a_probe_is_answered_with_what_adding_the_device_needs() {
    run(async {
        let b = beacon(4321).await;
        let device = probe(Ipv4Addr::LOCALHOST, b.port)
            .await
            .expect("the beacon answers");
        assert!(device.lan_mouse);
        assert_eq!(device.port, Some(4321));
        assert_eq!(device.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(device.fingerprint.as_deref(), Some("aa:bb:cc"));
        assert!(
            !device.fingerprint_verified,
            "a beacon claim is never verified"
        );
        b.stop().await;
    });
}

/// A burst of probes from one address gets one answer per interval: the
/// beacon cannot be used to reflect traffic at a forged source.
#[test]
fn a_probe_burst_from_one_address_gets_a_single_answer() {
    run(async {
        let b = beacon(4242).await;
        let socket = prober(b.port).await;
        for _ in 0..20 {
            socket.send(MAGIC).await.unwrap();
        }
        let answers = replies(&socket, Duration::from_millis(300)).await;
        assert_eq!(answers.len(), 1, "{} answers to one burst", answers.len());
        // after the interval it answers again
        tokio::time::sleep(Duration::from_millis(150)).await;
        socket.send(MAGIC).await.unwrap();
        assert_eq!(replies(&socket, Duration::from_millis(300)).await.len(), 1);
        b.stop().await;
    });
}

/// The answer is never larger than a few hundred bytes: it is sent to an
/// address nobody verified.
#[test]
fn the_answer_stays_small() {
    run(async {
        let b = beacon(4242).await;
        let socket = prober(b.port).await;
        socket.send(MAGIC).await.unwrap();
        let answers = replies(&socket, Duration::from_millis(300)).await;
        assert_eq!(answers.len(), 1);
        assert!(answers[0].len() < 512, "{} bytes", answers[0].len());
        b.stop().await;
    });
}

/// Garbage, a reply (instead of a probe) and broken announcements get no
/// answer and reach nothing; a real probe right after still works.
#[test]
fn garbage_gets_no_answer_and_breaks_nothing() {
    run(async {
        let mut b = beacon(4242).await;
        let socket = prober(b.port).await;
        let mut broken_announce = ANNOUNCE_MAGIC.to_vec();
        broken_announce.extend_from_slice(b"{\"port\":");
        let mut huge_announce = ANNOUNCE_MAGIC.to_vec();
        huge_announce.extend(std::iter::repeat_n(b'x', 1500));
        for packet in [
            b"".to_vec(),
            b"LMDISC0".to_vec(),
            b"HELLO WORLD".to_vec(),
            ANNOUNCE_MAGIC.to_vec(),
            broken_announce,
            huge_announce,
        ] {
            socket.send(&packet).await.unwrap();
        }
        assert!(
            replies(&socket, Duration::from_millis(200))
                .await
                .is_empty()
        );
        assert!(
            b.next_announcement(Duration::from_millis(100))
                .await
                .is_none()
        );

        // the beacon still answers a real probe afterwards
        tokio::time::sleep(Duration::from_millis(120)).await;
        socket.send(MAGIC).await.unwrap();
        assert_eq!(replies(&socket, Duration::from_millis(300)).await.len(), 1);
        b.stop().await;
    });
}

/// An announcement from another machine reaches the service with what it
/// claims; a second one from the same address within a second does not.
#[test]
fn announcements_are_forwarded_and_rate_limited_per_address() {
    run(async {
        let mut b = beacon(4242).await;
        let socket = prober(b.port).await;
        let announce = encode(
            ANNOUNCE_MAGIC,
            &BeaconReply {
                port: 5555,
                link: lan_mouse_ipc::LinkKind::Wireless,
                hostname: "outro-pc".into(),
                version: "9.9.9".into(),
                fingerprint: Some("11:22".into()),
            },
        )
        .unwrap();
        socket.send(&announce).await.unwrap();
        let first = b
            .next_announcement(Duration::from_millis(500))
            .await
            .expect("forwarded");
        assert_eq!(first.ip, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(first.device.hostname.as_deref(), Some("outro-pc"));
        assert_eq!(first.device.port, Some(5555));
        assert_eq!(first.device.fingerprint.as_deref(), Some("11:22"));
        assert!(!first.device.fingerprint_verified);

        socket.send(&announce).await.unwrap();
        assert!(
            b.next_announcement(Duration::from_millis(300))
                .await
                .is_none(),
            "a repeated announcement within the interval was forwarded"
        );
        // and an announcement never gets an answer (no reflection)
        assert!(
            replies(&socket, Duration::from_millis(100))
                .await
                .is_empty()
        );
        b.stop().await;
    });
}

/// A second instance on a port that is taken gives up quietly instead of
/// failing the service.
#[test]
fn a_taken_discovery_port_disables_discovery_quietly() {
    run(async {
        let b = beacon(4242).await;
        let (tx, _rx) = local_channel::mpsc::channel();
        let second = tokio::time::timeout(
            Duration::from_secs(2),
            beacon_task(
                4242,
                b.port,
                CancellationToken::new(),
                "x".into(),
                tx,
                false,
            ),
        )
        .await;
        assert!(
            second.is_ok(),
            "the second beacon neither returned nor failed"
        );
        b.stop().await;
    });
}

/// Nothing listening at all: the probe says nothing is there (no device,
/// or a plain host), never a Lan Mouse.
#[test]
fn a_probe_without_a_beacon_finds_no_lan_mouse() {
    run(async {
        let device = probe(Ipv4Addr::LOCALHOST, free_port()).await;
        assert!(device.is_none_or(|device| !device.lan_mouse));
    });
}
