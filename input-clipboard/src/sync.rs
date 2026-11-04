//! Clipboard sync orchestration: local changes out, remote snapshots in.
//!
//! The sync layer is transport agnostic. A [`FrameSink`] sends frames to a
//! peer; incoming frames are pushed in with [`ClipboardSync::handle_frame`].
//! The service implements the sink over the existing DTLS connection and
//! feeds frames received by the listener; tests use an in-memory queue.

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    rc::Rc,
    time::Instant,
};

use lan_mouse_proto::clipboard::ClipboardFrame;

use crate::{
    ClipboardSnapshot, Limits,
    transfer::{IncomingTransfer, OutgoingTransfer},
};

pub type PeerId = u64;

/// Outgoing frame transport.
pub trait FrameSink {
    fn send(&self, peer: PeerId, frame: ClipboardFrame);
}

/// One clipboard sync instance.
pub struct ClipboardSync<S: FrameSink> {
    sink: S,
    /// Shared with the service so config changes apply without a restart.
    limits: Rc<Cell<Limits>>,
    origin: u64,
    peers: HashSet<PeerId>,
    outgoing: HashMap<PeerId, OutgoingTransfer>,
    incoming: HashMap<PeerId, IncomingTransfer>,
    last_applied: Option<[u8; 32]>,
    next_serial: u32,
    last_error: Option<String>,
    applied: u64,
}

impl<S: FrameSink> ClipboardSync<S> {
    pub fn new(sink: S, limits: Rc<Cell<Limits>>, origin: u64) -> Self {
        Self {
            sink,
            limits,
            origin,
            peers: HashSet::new(),
            outgoing: HashMap::new(),
            incoming: HashMap::new(),
            last_applied: None,
            next_serial: 0,
            last_error: None,
            applied: 0,
        }
    }

    pub fn add_peer(&mut self, peer: PeerId) {
        self.peers.insert(peer);
    }

    pub fn remove_peer(&mut self, peer: PeerId) {
        self.peers.remove(&peer);
        self.outgoing.remove(&peer);
    }

    pub fn peers(&self) -> impl Iterator<Item = PeerId> + '_ {
        self.peers.iter().copied()
    }

    pub fn applied_count(&self) -> u64 {
        self.applied
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Drop every in-flight transfer (used when clipboard sync is disabled).
    pub fn reset(&mut self) {
        self.outgoing.clear();
        self.incoming.clear();
    }

    pub fn clear_error(&mut self) {
        self.last_error = None;
    }

    /// Start (or replace) a transfer for a locally copied snapshot.
    ///
    /// Snapshots that exceed the limits, are empty, or match the content last
    /// applied from a peer (echo) are ignored.
    pub fn on_local_change(&mut self, snapshot: ClipboardSnapshot, now: Instant) {
        if snapshot.is_empty() {
            return;
        }
        if !self.limits.get().accepts(&snapshot) {
            self.last_error = Some("clipboard snapshot exceeds the size limit".into());
            log::warn!("clipboard snapshot exceeds the size limit, not syncing");
            return;
        }
        let hash = snapshot.hash();
        if self.last_applied == Some(hash) {
            log::debug!("clipboard change originated from a peer, not syncing back");
            return;
        }
        if self.peers.is_empty() {
            return;
        }

        let serial = self.next_serial.wrapping_add(1);
        self.next_serial = serial;

        // Abort anything still in flight for the previous copy.
        for (peer, previous) in self.outgoing.drain() {
            self.sink.send(
                peer,
                ClipboardFrame::Cancel {
                    serial: previous.serial(),
                },
            );
        }

        for peer in self.peers.clone() {
            let mut transfer = OutgoingTransfer::new(serial, self.origin, &snapshot);
            let frames = transfer.poll(now);
            for frame in frames {
                self.sink.send(peer, frame);
            }
            self.outgoing.insert(peer, transfer);
        }
        log::info!(
            "clipboard: syncing {} bytes in {} format(s) to {} peer(s)",
            snapshot.total_len(),
            snapshot.len(),
            self.peers.len()
        );
    }

    pub fn handle_frame(&mut self, peer: PeerId, frame: ClipboardFrame, now: Instant) {
        match frame {
            ClipboardFrame::Offer {
                serial,
                origin,
                hash,
                items,
            } => {
                if origin == self.origin {
                    return;
                }
                if self.last_applied == Some(hash) {
                    return;
                }
                // A retransmitted offer for the transfer already in progress
                // must not reset it; just repeat the current acknowledgements.
                if let Some(existing) = self.incoming.get(&peer) {
                    if existing.serial() == serial {
                        for ack in existing.acks() {
                            self.sink.send(peer, ack);
                        }
                        return;
                    }
                }
                match IncomingTransfer::from_offer(
                    serial,
                    origin,
                    hash,
                    &items,
                    &self.limits.get(),
                    now,
                ) {
                    Ok(transfer) => {
                        log::debug!(
                            "clipboard: accepting snapshot {serial} with {} format(s)",
                            items.len()
                        );
                        self.incoming.insert(peer, transfer);
                    }
                    Err(e) => {
                        log::warn!("clipboard: rejecting offer: {e}");
                        self.last_error = Some(e.to_string());
                        self.sink.send(peer, ClipboardFrame::Cancel { serial });
                    }
                }
            }
            ClipboardFrame::Chunk {
                serial,
                mime,
                offset,
                total,
                data,
            } => {
                let Some(incoming) = self.incoming.get_mut(&peer) else {
                    return;
                };
                if incoming.serial() != serial {
                    return;
                }
                match incoming.on_chunk(serial, mime, offset, total, &data, now) {
                    Ok(Some(ack)) => self.sink.send(peer, ack),
                    Ok(None) => {}
                    Err(e) => {
                        log::warn!("clipboard: bad chunk: {e}");
                        self.incoming.remove(&peer);
                        self.sink.send(peer, ClipboardFrame::Cancel { serial });
                    }
                }
            }
            ClipboardFrame::Ack {
                serial,
                mime,
                base,
                bitmap,
            } => {
                let Some(transfer) = self.outgoing.get_mut(&peer) else {
                    return;
                };
                if transfer.on_ack(serial, mime, base, bitmap) {
                    let frames = transfer.poll(now);
                    for frame in frames {
                        self.sink.send(peer, frame);
                    }
                }
            }
            ClipboardFrame::Cancel { serial } => {
                if self.incoming.get(&peer).map(IncomingTransfer::serial) == Some(serial) {
                    self.incoming.remove(&peer);
                }
                if self.outgoing.get(&peer).map(OutgoingTransfer::serial) == Some(serial) {
                    self.outgoing.remove(&peer);
                }
            }
        }
    }

    /// Drive retransmissions and timeouts.
    ///
    /// Returns a completed remote snapshot that the caller must apply to the
    /// local clipboard and confirm with [`ClipboardSync::on_applied`].
    pub fn tick(&mut self, now: Instant) -> Option<ClipboardSnapshot> {
        let mut finished = Vec::new();
        let mut failed: Vec<(PeerId, u32)> = Vec::new();
        for (peer, transfer) in self.outgoing.iter_mut() {
            let frames = transfer.poll(now);
            for frame in frames {
                self.sink.send(*peer, frame);
            }
            if transfer.is_failed() {
                failed.push((*peer, transfer.serial()));
            } else if transfer.is_done() {
                finished.push(*peer);
            }
        }
        for (peer, serial) in failed {
            self.outgoing.remove(&peer);
            self.last_error = Some("clipboard transfer aborted".into());
            self.sink.send(peer, ClipboardFrame::Cancel { serial });
        }
        for peer in finished {
            self.outgoing.remove(&peer);
        }

        self.incoming.retain(|peer, transfer| {
            if transfer.timed_out(now) {
                log::warn!("clipboard: incoming transfer from peer {peer} timed out");
                false
            } else {
                true
            }
        });

        let completed = self
            .incoming
            .iter()
            .find(|(_, transfer)| transfer.is_complete())
            .map(|(peer, _)| *peer)?;
        let transfer = self.incoming.remove(&completed)?;
        match transfer.assemble() {
            Ok(snapshot) => Some(snapshot),
            Err(e) => {
                log::warn!("clipboard: rejected completed transfer: {e}");
                self.last_error = Some(e.to_string());
                None
            }
        }
    }

    /// Confirmation that a snapshot returned by [`ClipboardSync::tick`] was
    /// written to the local clipboard.
    pub fn on_applied(&mut self, snapshot: &ClipboardSnapshot) {
        self.last_applied = Some(snapshot.hash());
        self.applied += 1;
        log::info!(
            "clipboard: applied {} bytes in {} format(s) from a peer",
            snapshot.total_len(),
            snapshot.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque, rc::Rc};

    use super::*;
    use crate::{Format, SharedClipboard};

    type Wire = Rc<RefCell<VecDeque<(PeerId, PeerId, ClipboardFrame)>>>;

    #[derive(Clone)]
    struct TestSink {
        from: PeerId,
        wire: Wire,
    }

    impl FrameSink for TestSink {
        fn send(&self, peer: PeerId, frame: ClipboardFrame) {
            self.wire.borrow_mut().push_back((self.from, peer, frame));
        }
    }

    fn snapshot() -> ClipboardSnapshot {
        ClipboardSnapshot::from_items([
            (Format::Text, b"copy me".to_vec()),
            (Format::Html, b"<i>copy me</i>".to_vec()),
            (
                Format::Png,
                vec![0x11; lan_mouse_proto::clipboard::CHUNK_SIZE * 2 + 9],
            ),
        ])
    }

    struct Harness {
        wire: Wire,
        a: ClipboardSync<TestSink>,
        b: ClipboardSync<TestSink>,
        clip_a: SharedClipboard,
        clip_b: SharedClipboard,
        now: Instant,
        seed: u64,
    }

    impl Harness {
        fn new() -> Self {
            let wire: Wire = Default::default();
            let clip_a = SharedClipboard::new();
            let clip_b = SharedClipboard::new();
            let mut a = ClipboardSync::new(
                TestSink {
                    from: 1,
                    wire: wire.clone(),
                },
                Rc::new(Cell::new(Limits::default())),
                0xaaaa,
            );
            let mut b = ClipboardSync::new(
                TestSink {
                    from: 2,
                    wire: wire.clone(),
                },
                Rc::new(Cell::new(Limits::default())),
                0xbbbb,
            );
            a.add_peer(2);
            b.add_peer(1);
            Self {
                wire,
                a,
                b,
                clip_a,
                clip_b,
                now: Instant::now(),
                seed: 0x9e37_79b9_7f4a_7c15,
            }
        }

        /// Deliver queued frames, letting `drop_rate` percent be lost.
        async fn step(&mut self, drop_rate: u32) {
            let frames = self.wire.borrow_mut().drain(..).collect::<Vec<_>>();
            for (from, to, frame) in frames {
                self.seed = self.seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                if drop_rate > 0 && (self.seed % 100) < drop_rate as u64 {
                    continue;
                }
                let target = if to == 2 { &mut self.b } else { &mut self.a };
                target.handle_frame(from, frame, self.now);
            }
            if let Some(snapshot) = self.a.tick(self.now) {
                self.clip_a.apply_remote(&snapshot);
                self.a.on_applied(&snapshot);
            }
            if let Some(snapshot) = self.b.tick(self.now) {
                self.clip_b.apply_remote(&snapshot);
                self.b.on_applied(&snapshot);
            }
            self.now += std::time::Duration::from_millis(10);
        }

        async fn run_until_applied(&mut self, drop_rate: u32, max_steps: usize) {
            for _ in 0..max_steps {
                self.step(drop_rate).await;
                if self.b.applied_count() > 0 {
                    return;
                }
            }
            panic!("clipboard did not synchronize in {max_steps} steps");
        }
    }

    #[tokio::test]
    async fn copy_on_a_pastes_on_b() {
        let mut h = Harness::new();
        h.clip_a.set_local(snapshot());
        h.a.on_local_change(snapshot(), h.now);
        h.run_until_applied(0, 5_000).await;
        assert_eq!(h.clip_b.current(), snapshot());
        assert_eq!(h.b.applied_count(), 1);
    }

    #[tokio::test]
    async fn survives_chunk_and_ack_loss() {
        let mut h = Harness::new();
        h.a.on_local_change(snapshot(), h.now);
        h.run_until_applied(20, 20_000).await;
        assert_eq!(h.clip_b.current(), snapshot());
    }

    #[tokio::test]
    async fn applied_remote_snapshot_is_not_echoed_back() {
        let mut h = Harness::new();
        h.a.on_local_change(snapshot(), h.now);
        h.run_until_applied(0, 5_000).await;
        h.wire.borrow_mut().clear();

        // The local watcher fires with exactly the content we just applied.
        h.b.on_local_change(h.clip_b.current(), h.now);
        assert!(h.wire.borrow().is_empty(), "remote content was echoed back");
    }

    #[tokio::test]
    async fn oversized_snapshot_is_not_sent() {
        let mut h = Harness::new();
        let big = ClipboardSnapshot::from_items([(Format::Png, vec![0; 9 * 1024 * 1024])]);
        h.a.on_local_change(big, h.now);
        assert!(h.wire.borrow().is_empty());
        assert!(h.a.last_error().is_some());
    }

    #[tokio::test]
    async fn a_new_copy_cancels_the_in_flight_transfer() {
        let mut h = Harness::new();
        h.a.on_local_change(snapshot(), h.now);
        // deliver only the offer + a few chunks
        for _ in 0..3 {
            h.step(0).await;
        }
        let replacement = ClipboardSnapshot::from_items([(Format::Text, b"replacement".to_vec())]);
        h.a.on_local_change(replacement.clone(), h.now);
        h.run_until_applied(0, 5_000).await;
        assert_eq!(h.clip_b.current(), replacement);
    }

    #[tokio::test]
    async fn one_local_copy_reaches_every_peer() {
        let wire: Wire = Default::default();
        let clip_a = SharedClipboard::new();
        let clip_b = SharedClipboard::new();
        let clip_c = SharedClipboard::new();
        let mut a = ClipboardSync::new(
            TestSink {
                from: 1,
                wire: wire.clone(),
            },
            Rc::new(Cell::new(Limits::default())),
            1,
        );
        let mut b = ClipboardSync::new(
            TestSink {
                from: 2,
                wire: wire.clone(),
            },
            Rc::new(Cell::new(Limits::default())),
            2,
        );
        let mut c = ClipboardSync::new(
            TestSink {
                from: 3,
                wire: wire.clone(),
            },
            Rc::new(Cell::new(Limits::default())),
            3,
        );
        a.add_peer(2);
        a.add_peer(3);
        b.add_peer(1);
        c.add_peer(1);

        a.on_local_change(snapshot(), Instant::now());
        let mut now = Instant::now();
        for _ in 0..20_000 {
            let frames = wire.borrow_mut().drain(..).collect::<Vec<_>>();
            for (from, to, frame) in frames {
                match to {
                    2 => b.handle_frame(from, frame, now),
                    3 => c.handle_frame(from, frame, now),
                    _ => a.handle_frame(from, frame, now),
                }
            }
            for (sync, clip) in [(&mut a, &clip_a), (&mut b, &clip_b), (&mut c, &clip_c)] {
                if let Some(snapshot) = sync.tick(now) {
                    clip.apply_remote(&snapshot);
                    sync.on_applied(&snapshot);
                }
            }
            now += std::time::Duration::from_millis(10);
            if b.applied_count() > 0 && c.applied_count() > 0 {
                break;
            }
        }
        assert_eq!(clip_b.current(), snapshot());
        assert_eq!(clip_c.current(), snapshot());
    }

    #[tokio::test]
    async fn empty_snapshot_is_ignored() {
        let mut h = Harness::new();
        h.a.on_local_change(ClipboardSnapshot::default(), h.now);
        assert!(h.wire.borrow().is_empty());
    }
}
