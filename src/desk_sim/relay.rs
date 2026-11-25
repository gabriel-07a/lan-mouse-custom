//! A bad network between two simulated PCs: a UDP relay in front of a
//! service's listen port that loses, delays (and so reorders) and duplicates
//! datagrams in both directions, from a seed so a failure replays.

use std::{
    cell::RefCell,
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    rc::Rc,
    time::Duration,
};

use tokio::{
    net::UdpSocket,
    task::{JoinHandle, spawn_local},
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Impairment {
    /// share of datagrams dropped (0.0..1.0)
    pub(crate) loss: f64,
    /// each datagram is delayed by up to this much (delays reorder)
    pub(crate) jitter: Duration,
    /// share of datagrams delivered twice
    pub(crate) duplicate: f64,
}

/// Deterministic per-relay randomness.
struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

type SharedRng = Rc<RefCell<Rng>>;

/// Start a relay to `target_port`; returns the port to dial instead.
pub(crate) async fn spawn(
    target_port: u16,
    impairment: Impairment,
    seed: u64,
) -> (u16, JoinHandle<()>) {
    let front = Rc::new(
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("relay socket"),
    );
    let port = front.local_addr().expect("relay addr").port();
    let rng: SharedRng = Rc::new(RefCell::new(Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15))));
    let task = spawn_local(async move {
        // one upstream socket per client, so replies find their way back
        let mut upstreams: HashMap<SocketAddr, Rc<UdpSocket>> = HashMap::new();
        let target = SocketAddr::from((Ipv4Addr::LOCALHOST, target_port));
        let mut buf = vec![0u8; 65_536];
        loop {
            let Ok((n, client)) = front.recv_from(&mut buf).await else {
                continue;
            };
            let upstream = match upstreams.get(&client) {
                Some(upstream) => upstream.clone(),
                None => {
                    let upstream = Rc::new(
                        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
                            .await
                            .expect("relay upstream"),
                    );
                    if upstream.connect(target).await.is_err() {
                        continue;
                    }
                    upstreams.insert(client, upstream.clone());
                    // replies: target -> client, impaired too
                    let (front, back, rng) = (front.clone(), upstream.clone(), rng.clone());
                    spawn_local(async move {
                        let mut buf = vec![0u8; 65_536];
                        while let Ok(n) = back.recv(&mut buf).await {
                            let datagram = buf[..n].to_vec();
                            let front = front.clone();
                            deliver(&rng, impairment, datagram, move |d| {
                                let front = front.clone();
                                async move {
                                    let _ = front.send_to(&d, client).await;
                                }
                            });
                        }
                    });
                    upstream
                }
            };
            let datagram = buf[..n].to_vec();
            deliver(&rng, impairment, datagram, move |d| {
                let upstream = upstream.clone();
                async move {
                    let _ = upstream.send(&d).await;
                }
            });
        }
    });
    (port, task)
}

/// Send `datagram` through `send`, subject to `impairment`.
fn deliver<F, Fut>(rng: &SharedRng, impairment: Impairment, datagram: Vec<u8>, send: F)
where
    F: Fn(Vec<u8>) -> Fut + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    let (drop, copies, delays) = {
        let mut rng = rng.borrow_mut();
        let drop = rng.unit() < impairment.loss;
        let copies = if rng.unit() < impairment.duplicate {
            2
        } else {
            1
        };
        let delays: Vec<Duration> = (0..copies)
            .map(|_| impairment.jitter.mul_f64(rng.unit()))
            .collect();
        (drop, copies, delays)
    };
    if drop {
        return;
    }
    let send = Rc::new(send);
    for delay in delays.into_iter().take(copies) {
        let (send, datagram) = (send.clone(), datagram.clone());
        if delay.is_zero() {
            spawn_local(async move { send(datagram).await });
        } else {
            spawn_local(async move {
                tokio::time::sleep(delay).await;
                send(datagram).await;
            });
        }
    }
}
