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
    time::{Duration, Instant},
};

use lan_mouse_proto::clipboard::{CHUNK_SIZE, ClipboardFrame};

use crate::{
    ClipboardSnapshot, Limits,
    transfer::{IncomingTransfer, OutgoingTransfer},
};

pub type PeerId = u64;

/// How long to wait before automatically re-offering a copy whose transfer to
/// a peer failed (aborted or cancelled). A short link outage should not cost
/// the user's copy; the re-offer is bounded by [`MAX_RETRIES`].
pub const RETRY_INTERVAL: Duration = Duration::from_millis(2_500);
/// Maximum automatic re-offers of one failed copy.
pub const MAX_RETRIES: u8 = 20;

/// Outgoing frame transport.
pub trait FrameSink {
    fn send(&self, peer: PeerId, frame: ClipboardFrame);
}

/// A copy that could not be delivered to some peers and is re-offered
/// automatically after [`RETRY_INTERVAL`].
struct PendingRetry {
    snapshot: ClipboardSnapshot,
    peers: HashSet<PeerId>,
    attempts: u8,
    next_at: Instant,
}

/// A completed incoming transfer whose final acknowledgement is withheld
/// until the service committed the snapshot to the local clipboard.
///
/// An acknowledgement therefore means "received *and applied*": a receiver
/// that cannot write the clipboard leaves the sender's transfer in flight, so
/// the sender retries or re-offers instead of believing a copy was delivered.
struct DeferredCommit {
    peer: PeerId,
    serial: u32,
    hash: [u8; 32],
    acks: Vec<ClipboardFrame>,
}

/// One clipboard sync instance.
pub struct ClipboardSync<S: FrameSink> {
    sink: S,
    /// Shared with the service so config changes apply without a restart.
    limits: Rc<Cell<Limits>>,
    origin: u64,
    /// Registered peer identities. Frames (including late datagrams queued
    /// before a disconnect) are only accepted from these peers.
    peers: HashSet<PeerId>,
    /// Subset of [`Self::peers`] that may receive our clipboard. A write-only
    /// device (granted clipboard-write but not clipboard-read) is still
    /// registered -- so its inbound content and acknowledgements are accepted
    /// -- but our content is never offered to it.
    may_receive: HashSet<PeerId>,
    outgoing: HashMap<PeerId, OutgoingTransfer>,
    incoming: HashMap<PeerId, IncomingTransfer>,
    /// Hash of the last snapshot written to the local clipboard on behalf of a
    /// peer. Suppresses the backend's owner-change echo of that write.
    last_applied: Option<[u8; 32]>,
    /// Hash last offered to each peer. A single global `last_sent` hash used to
    /// swallow the read-on-connect resync after a reconnect (the service reads
    /// the clipboard and re-offers it, which looked like a duplicate change
    /// event), so a copy interrupted by the disconnect was never retried.
    offered: HashMap<PeerId, [u8; 32]>,
    /// Final acknowledgements withheld until the app applied the snapshot.
    deferred: Vec<DeferredCommit>,
    /// Automatic re-offer of copies whose transfer failed.
    retry: Option<PendingRetry>,
    next_serial: u32,
    last_error: Option<String>,
    applied: u64,
}

/// First serial of a sync instance.
///
/// Every sync instance (including one recreated after a clipboard backend
/// restart) gets its own serial epoch. Otherwise a receiver that still holds a
/// transfer from the previous instance could re-acknowledge the new instance's
/// first offer by serial collision alone, completing it without any data.
fn serial_epoch(origin: u64) -> u32 {
    let mixed = origin ^ (origin >> 32);
    (mixed as u32) ^ ((mixed >> 16) as u32) ^ 0x9e37_79b9
}

fn chunk_count(size: u32) -> u32 {
    size.div_ceil(CHUNK_SIZE as u32)
}

impl<S: FrameSink> ClipboardSync<S> {
    pub fn new(sink: S, limits: Rc<Cell<Limits>>, origin: u64) -> Self {
        Self {
            sink,
            limits,
            origin,
            peers: HashSet::new(),
            may_receive: HashSet::new(),
            outgoing: HashMap::new(),
            incoming: HashMap::new(),
            last_applied: None,
            offered: HashMap::new(),
            deferred: Vec::new(),
            retry: None,
            next_serial: serial_epoch(origin),
            last_error: None,
            applied: 0,
        }
    }

    fn next_serial(&mut self) -> u32 {
        self.next_serial = self.next_serial.wrapping_add(1);
        self.next_serial
    }

    pub fn add_peer(&mut self, peer: PeerId) {
        self.peers.insert(peer);
        self.may_receive.insert(peer);
    }

    /// Whether `peer` may receive our clipboard (it advertised the extension
    /// and the device policy grants clipboard-read). Registered peers that may
    /// not receive still accept inbound content and acks.
    pub fn set_peer_may_receive(&mut self, peer: PeerId, may_receive: bool) {
        if !self.peers.contains(&peer) {
            return;
        }
        if may_receive {
            self.may_receive.insert(peer);
            return;
        }
        self.may_receive.remove(&peer);
        self.offered.remove(&peer);
        if let Some(previous) = self.outgoing.remove(&peer) {
            self.sink.send(
                peer,
                ClipboardFrame::Cancel {
                    serial: previous.serial(),
                },
            );
        }
        if let Some(retry) = &mut self.retry {
            retry.peers.remove(&peer);
            if retry.peers.is_empty() {
                self.retry = None;
            }
        }
    }

    fn receiving_peers(&self) -> impl Iterator<Item = PeerId> + '_ {
        self.peers
            .iter()
            .copied()
            .filter(|peer| self.may_receive.contains(peer))
    }

    pub fn remove_peer(&mut self, peer: PeerId) {
        self.peers.remove(&peer);
        self.may_receive.remove(&peer);
        self.outgoing.remove(&peer);
        // Frames queued before the disconnect must not complete an incoming
        // transfer after the peer is gone (a late chunk could otherwise apply
        // stale content).
        self.incoming.remove(&peer);
        self.offered.remove(&peer);
        self.deferred.retain(|commit| commit.peer != peer);
        if let Some(retry) = &mut self.retry {
            retry.peers.remove(&peer);
            if retry.peers.is_empty() {
                self.retry = None;
            }
        }
    }

    /// Move the sync state of `from` to `to`.
    ///
    /// The service registers the same peer twice in some connection orders: an
    /// address keyed peer for frames that arrived before the outgoing
    /// connection completed its `Hello`, then the client handle once the
    /// capabilities are known. Migrating instead of dropping keeps in-flight
    /// transfers and acknowledgements attributed to one identity.
    pub fn migrate_peer(&mut self, from: PeerId, to: PeerId) {
        if from == to {
            return;
        }
        if self.peers.remove(&from) {
            self.peers.insert(to);
            if self.may_receive.remove(&from) {
                self.may_receive.insert(to);
            }
        }
        if let Some(transfer) = self.outgoing.remove(&from) {
            self.outgoing.insert(to, transfer);
        }
        if let Some(transfer) = self.incoming.remove(&from) {
            self.incoming.insert(to, transfer);
        }
        if let Some(hash) = self.offered.remove(&from) {
            self.offered.insert(to, hash);
        }
        for commit in &mut self.deferred {
            if commit.peer == from {
                commit.peer = to;
            }
        }
        if let Some(retry) = &mut self.retry {
            if retry.peers.remove(&from) {
                retry.peers.insert(to);
            }
        }
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
        self.offered.clear();
        self.deferred.clear();
        self.retry = None;
    }

    pub fn clear_error(&mut self) {
        self.last_error = None;
    }

    /// Offer the local clipboard to one peer, bypassing the duplicate-change
    /// deduplication of [`ClipboardSync::on_local_change`].
    ///
    /// The service calls this for every peer registration (fresh connection,
    /// reconnect, incoming session, backend restart): the peer has not seen the
    /// current clipboard, even when the local hash did not change since it was
    /// last broadcast to the other peers.
    pub fn offer_current_to(&mut self, peer: PeerId, snapshot: &ClipboardSnapshot, now: Instant) {
        if !self.may_receive.contains(&peer) || snapshot.is_empty() {
            return;
        }
        if !self.limits.get().accepts(snapshot) {
            self.last_error = Some("clipboard snapshot exceeds the size limit".into());
            log::warn!("clipboard snapshot exceeds the size limit, not syncing");
            return;
        }
        let hash = snapshot.hash();
        self.start_transfer(peer, snapshot, hash, now);
        if let Some(retry) = &mut self.retry {
            retry.peers.remove(&peer);
            if retry.peers.is_empty() {
                self.retry = None;
            }
        }
        log::info!(
            "clipboard: offering the current clipboard ({} bytes) to peer {peer}",
            snapshot.total_len()
        );
    }

    /// Start (or replace) a transfer to one peer.
    fn start_transfer(
        &mut self,
        peer: PeerId,
        snapshot: &ClipboardSnapshot,
        hash: [u8; 32],
        now: Instant,
    ) {
        if let Some(previous) = self.outgoing.remove(&peer) {
            self.sink.send(
                peer,
                ClipboardFrame::Cancel {
                    serial: previous.serial(),
                },
            );
        }
        let serial = self.next_serial();
        let mut transfer = OutgoingTransfer::new(serial, self.origin, snapshot);
        for frame in transfer.poll(now) {
            self.sink.send(peer, frame);
        }
        self.outgoing.insert(peer, transfer);
        self.offered.insert(peer, hash);
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
        // A genuinely new local copy replaces whatever the peer put on the
        // clipboard, so a later re-copy of exactly that content is a real copy
        // and must be offered instead of being mistaken for an echo.
        self.last_applied = None;
        // A peer only needs the content if it may receive our clipboard and
        // has not been offered that exact hash yet. This keeps duplicate
        // compositor events cheap while still offering the clipboard to a peer
        // that joined since the last copy.
        let pending: Vec<PeerId> = self
            .receiving_peers()
            .filter(|peer| self.offered.get(peer) != Some(&hash))
            .collect();
        if pending.is_empty() {
            if self.receiving_peers().next().is_none() {
                log::debug!("clipboard: local change with no peers, not offering it");
            } else {
                log::debug!("clipboard: ignoring a duplicate change event");
            }
            return;
        }
        // A new copy supersedes a pending automatic re-offer.
        self.retry = None;
        for peer in pending {
            self.start_transfer(peer, &snapshot, hash, now);
        }
        log::info!(
            "clipboard: syncing {} bytes in {} format(s) to {} peer(s)",
            snapshot.total_len(),
            snapshot.len(),
            self.peers.len()
        );
    }

    pub fn handle_frame(&mut self, peer: PeerId, frame: ClipboardFrame, now: Instant) {
        // Frames are only accepted from registered peers. A datagram queued
        // before a disconnect (or a replayed frame) must not recreate a
        // transfer and apply stale content after the peer is gone. Write-only
        // peers are registered too, so their inbound content still passes.
        if !self.peers.contains(&peer) {
            log::trace!("clipboard: ignoring a frame from unregistered peer {peer}");
            return;
        }
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
                    // This content is already on the local clipboard. A
                    // sender that re-offers it (lost final acknowledgement, a
                    // retried copy) must not transfer it again; acknowledge
                    // it as complete instead of leaving it retransmitting.
                    log::debug!(
                        "clipboard: peer {peer} re-offers already applied content, acknowledging"
                    );
                    for (mime, size) in items {
                        self.sink.send(
                            peer,
                            ClipboardFrame::Ack {
                                serial,
                                mime,
                                base: chunk_count(size),
                                bitmap: 0,
                            },
                        );
                    }
                    return;
                }
                // A retransmitted offer for the transfer already in progress
                // must not reset it; just repeat the current acknowledgements.
                // The origin and the hash must match too: serials are only
                // unique within one sync instance, and a receiver that still
                // holds a transfer from a previous instance must not fake
                // acknowledgements for an unrelated snapshot.
                if let Some(existing) = self.incoming.get(&peer) {
                    if existing.serial() == serial
                        && existing.origin() == origin
                        && existing.hash() == hash
                    {
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
                    Ok(Some(ack)) => {
                        if incoming.is_complete() {
                            // Withhold the final acknowledgement until the
                            // snapshot was committed to the local clipboard
                            // (see `on_applied`): an ack then means "applied",
                            // not merely "received", so a receiver whose
                            // clipboard write fails does not leave the sender
                            // believing the copy was delivered.
                            let hash = incoming.hash();
                            let serial = incoming.serial();
                            let acks = incoming.acks();
                            if !self
                                .deferred
                                .iter()
                                .any(|commit| commit.peer == peer && commit.serial == serial)
                            {
                                self.deferred.push(DeferredCommit {
                                    peer,
                                    serial,
                                    hash,
                                    acks,
                                });
                            }
                        } else {
                            self.sink.send(peer, ack);
                        }
                    }
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
                    log::trace!(
                        "clipboard: ack for serial {serial} from peer {peer} without a transfer"
                    );
                    return;
                };
                let accepted = transfer.on_ack(serial, mime, base, bitmap);
                log::trace!(
                    "clipboard: ack serial {serial} mime {mime} from peer {peer} (accepted: {accepted})"
                );
                if accepted {
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
                    let transfer = self.outgoing.remove(&peer).expect("checked above");
                    self.offered.remove(&peer);
                    log::warn!(
                        "clipboard: transfer {serial} to peer {peer} was cancelled by the peer"
                    );
                    self.last_error = Some("clipboard transfer cancelled by the peer".into());
                    // A cancellation is usually a temporary condition (the
                    // peer's limits, a restart); re-offer later instead of
                    // dropping the copy silently.
                    self.schedule_retry(transfer.snapshot(), [peer].into(), now);
                }
            }
        }
    }

    /// Re-offer failed copies that are due.
    fn run_due_retries(&mut self, now: Instant) {
        let Some(retry) = self.retry.take() else {
            return;
        };
        if now < retry.next_at {
            self.retry = Some(retry);
            return;
        }
        let peers: Vec<PeerId> = retry
            .peers
            .iter()
            .copied()
            .filter(|peer| self.may_receive.contains(peer))
            .collect();
        if peers.is_empty() {
            return;
        }
        if retry.attempts >= MAX_RETRIES {
            log::warn!("clipboard: giving up re-offering a copy that could not be delivered");
            self.last_error = Some("clipboard transfer could not be delivered".into());
            return;
        }
        let hash = retry.snapshot.hash();
        for peer in &peers {
            self.start_transfer(*peer, &retry.snapshot, hash, now);
        }
        log::info!(
            "clipboard: re-offering a copy that could not be delivered (retry {}/{MAX_RETRIES})",
            retry.attempts + 1
        );
        self.retry = Some(PendingRetry {
            snapshot: retry.snapshot,
            peers: peers.into_iter().collect(),
            attempts: retry.attempts + 1,
            next_at: now + RETRY_INTERVAL,
        });
    }

    /// Keep retrying to deliver `snapshot` to `peers` in the background.
    fn schedule_retry(
        &mut self,
        snapshot: ClipboardSnapshot,
        peers: HashSet<PeerId>,
        now: Instant,
    ) {
        if peers.is_empty() {
            return;
        }
        match self.retry.as_mut() {
            Some(retry) if retry.snapshot.hash() == snapshot.hash() => {
                retry.peers.extend(peers);
                retry.next_at = now + RETRY_INTERVAL;
            }
            _ => {
                self.retry = Some(PendingRetry {
                    snapshot,
                    peers,
                    attempts: 0,
                    next_at: now + RETRY_INTERVAL,
                });
            }
        }
    }

    /// Drive retransmissions, automatic re-offers and timeouts.
    ///
    /// Returns a completed remote snapshot that the caller must apply to the
    /// local clipboard and confirm with [`ClipboardSync::on_applied`] (or
    /// abandon with [`ClipboardSync::on_apply_failed`]).
    pub fn tick(&mut self, now: Instant) -> Option<ClipboardSnapshot> {
        self.run_due_retries(now);

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

        let mut retry_snapshot: Option<ClipboardSnapshot> = None;
        let mut retry_peers: HashSet<PeerId> = HashSet::new();
        if !failed.is_empty() {
            for (peer, serial) in failed {
                if let Some(transfer) = self.outgoing.remove(&peer) {
                    if retry_snapshot.is_none() {
                        retry_snapshot = Some(transfer.snapshot());
                    }
                }
                // Allow a fresh copy (or the automatic re-offer below) to
                // reach this peer again.
                self.offered.remove(&peer);
                retry_peers.insert(peer);
                log::warn!("clipboard: transfer {serial} to peer {peer} failed");
                self.last_error = Some("clipboard transfer aborted".into());
                self.sink.send(peer, ClipboardFrame::Cancel { serial });
            }
        }
        if let Some(snapshot) = retry_snapshot {
            self.schedule_retry(snapshot, retry_peers, now);
        }

        for peer in finished {
            self.outgoing.remove(&peer);
            if let Some(retry) = &mut self.retry {
                retry.peers.remove(&peer);
                if retry.peers.is_empty() {
                    self.retry = None;
                }
            }
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
                // The peer must not stay with a dangling offer: cancel the
                // transfer so it stops retransmitting, and drop the withheld
                // acknowledgement.
                self.deferred.retain(|commit| {
                    commit.peer != completed || commit.serial != transfer.serial()
                });
                self.sink.send(
                    completed,
                    ClipboardFrame::Cancel {
                        serial: transfer.serial(),
                    },
                );
                None
            }
        }
    }

    /// Confirmation that a snapshot returned by [`ClipboardSync::tick`] was
    /// written to the local clipboard.
    pub fn on_applied(&mut self, snapshot: &ClipboardSnapshot) {
        let hash = snapshot.hash();
        self.last_applied = Some(hash);
        // A remote snapshot we just applied is the last-writer event. Copies
        // that are still in flight (or waiting for an automatic re-offer) hold
        // *older* content and must not overwrite the newer content on a peer:
        // cancel them and drop the pending re-offer. This is the divergence
        // seen when a copy made during a link outage was retried after the peer
        // had already copied something newer.
        let mut superseded: Vec<(PeerId, u32)> = Vec::new();
        self.outgoing.retain(|peer, transfer| {
            if transfer.hash() == hash {
                true
            } else {
                superseded.push((*peer, transfer.serial()));
                false
            }
        });
        for (peer, serial) in superseded {
            log::debug!(
                "clipboard: cancelling superseded transfer {serial} to peer {peer} after applying a remote copy"
            );
            self.sink.send(peer, ClipboardFrame::Cancel { serial });
        }
        self.drop_superseded_retry(&hash);
        // The local clipboard no longer holds the content last sent: a later
        // copy of that same content (e.g. re-copying an older snippet after
        // the peer overwrote our clipboard) must be offered again instead of
        // being mistaken for a duplicate owner-change event.
        self.offered.clear();
        self.applied += 1;
        // Release the withheld final acknowledgements now that the content is
        // really on the clipboard.
        let (committed, remaining): (Vec<_>, Vec<_>) = std::mem::take(&mut self.deferred)
            .into_iter()
            .partition(|commit| commit.hash == hash);
        self.deferred = remaining;
        for commit in committed {
            for ack in commit.acks {
                self.sink.send(commit.peer, ack);
            }
            log::debug!(
                "clipboard: committed content to the local clipboard, acknowledged to peer {}",
                commit.peer
            );
        }
        log::info!(
            "clipboard: applied {} bytes in {} format(s) from a peer",
            snapshot.total_len(),
            snapshot.len()
        );
    }

    /// The local clipboard backend refused to store a snapshot returned by
    /// [`ClipboardSync::tick`].
    ///
    /// The withheld final acknowledgements are dropped and the senders are
    /// told to abort (they automatically re-offer later), so nobody keeps a
    /// transfer alive for content that will never be applied.
    pub fn on_apply_failed(&mut self, snapshot: &ClipboardSnapshot) {
        let hash = snapshot.hash();
        self.drop_superseded_retry(&hash);
        let (failed, remaining): (Vec<_>, Vec<_>) = std::mem::take(&mut self.deferred)
            .into_iter()
            .partition(|commit| commit.hash == hash);
        self.deferred = remaining;
        for commit in failed {
            log::warn!(
                "clipboard: could not apply content from peer {}; cancelling its transfer",
                commit.peer
            );
            self.sink.send(
                commit.peer,
                ClipboardFrame::Cancel {
                    serial: commit.serial,
                },
            );
        }
    }

    /// Forget an automatic re-offer whose content is superseded by a snapshot
    /// with a different hash.
    fn drop_superseded_retry(&mut self, hash: &[u8; 32]) {
        if self
            .retry
            .as_ref()
            .is_some_and(|retry| retry.snapshot.hash() != *hash)
        {
            log::debug!("clipboard: dropping a pending re-offer superseded by newer content");
            self.retry = None;
        }
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

    /// After a remote snapshot replaced the local clipboard, copying the
    /// content this machine sent earlier must be offered again: otherwise the
    /// peer keeps the newer remote content and the two clipboards stay out of
    /// sync until the user copies something different.
    #[tokio::test]
    async fn recopying_previously_sent_content_is_offered_after_a_remote_apply() {
        let mut h = Harness::new();
        h.a.on_local_change(snapshot(), h.now);
        h.run_until_applied(0, 5_000).await;

        // The peer copies something else; it becomes the local clipboard.
        let replacement = ClipboardSnapshot::from_items([(Format::Text, b"from b".to_vec())]);
        h.b.on_local_change(replacement.clone(), h.now);
        for _ in 0..5_000 {
            h.step(0).await;
            if h.a.applied_count() > 0 {
                break;
            }
        }
        assert_eq!(h.clip_a.current(), replacement);

        // The user now re-copies the older content on this machine.
        h.wire.borrow_mut().clear();
        h.a.on_local_change(snapshot(), h.now);
        assert!(
            h.wire
                .borrow()
                .iter()
                .any(|(_, _, frame)| matches!(frame, ClipboardFrame::Offer { .. })),
            "previously sent content must be offered again after a remote apply"
        );
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

    /// Verifier-reported divergence: a copy scheduled for automatic re-offer
    /// during a link outage must not be re-offered after a *newer* remote copy
    /// was applied locally, otherwise the peer ends up with the stale content
    /// while the local clipboard holds the newer one.
    #[tokio::test]
    async fn stale_retry_does_not_overwrite_content_received_after_the_retry_was_scheduled() {
        let mut h = Harness::new();
        let older = ClipboardSnapshot::from_items([(Format::Text, b"older copy X".to_vec())]);
        let newer = ClipboardSnapshot::from_items([(Format::Text, b"newer copy Y".to_vec())]);
        h.a.on_local_change(older, h.now);

        // Link outage: every frame is lost until the transfer aborts and an
        // automatic re-offer is scheduled (+2.5 s). Stay below that deadline.
        for _ in 0..380 {
            h.step(100).await;
        }
        assert!(
            h.a.last_error().is_some(),
            "the transfer should have failed during the outage"
        );

        // The peer has copied newer content and we apply it locally.
        h.clip_b.set_local(newer.clone());
        h.clip_a.apply_remote(&newer);
        h.a.on_applied(&newer);
        h.wire.borrow_mut().clear();

        // Well past the retry deadline: the older copy must not be re-offered.
        for _ in 0..600 {
            h.step(0).await;
        }
        assert!(
            h.wire.borrow().is_empty(),
            "a stale retry re-offered content superseded by a newer remote copy"
        );
        assert_eq!(
            h.clip_b.current(),
            newer,
            "the peer was overwritten with stale content"
        );
    }
}

#[cfg(test)]
mod dedupe_tests {
    use std::{cell::RefCell, collections::VecDeque, rc::Rc};

    use super::*;
    use crate::Format;
    use lan_mouse_proto::clipboard::MIME_TEXT;

    type Wire = Rc<RefCell<VecDeque<(PeerId, ClipboardFrame)>>>;

    #[derive(Clone)]
    struct Sink {
        wire: Wire,
    }

    impl FrameSink for Sink {
        fn send(&self, _peer: PeerId, frame: ClipboardFrame) {
            self.wire.borrow_mut().push_back((_peer, frame));
        }
    }

    fn snapshot() -> ClipboardSnapshot {
        ClipboardSnapshot::from_items([(Format::Text, b"same text".to_vec())])
    }

    fn sync_with_peer(wire: &Wire) -> ClipboardSync<Sink> {
        let mut sync = ClipboardSync::new(
            Sink { wire: wire.clone() },
            Rc::new(Cell::new(Limits::default())),
            0x1234,
        );
        sync.add_peer(2);
        sync
    }

    fn offers(wire: &Wire) -> usize {
        wire.borrow()
            .iter()
            .filter(|(_, frame)| matches!(frame, ClipboardFrame::Offer { .. }))
            .count()
    }

    /// The compositor reports one change per MIME update; the same content
    /// must not be offered two or three times.
    #[test]
    fn duplicate_change_events_are_offered_once() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        let now = Instant::now();
        sync.on_local_change(snapshot(), now);
        sync.on_local_change(snapshot(), now);
        sync.on_local_change(snapshot(), now);
        assert_eq!(offers(&wire), 1);
    }

    /// A copy made while no peer is connected must still be sent once the
    /// peer shows up (the hash must not be recorded with nobody to send to).
    #[test]
    fn change_without_peers_is_offered_when_a_peer_appears() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        sync.remove_peer(2);

        let now = Instant::now();
        sync.on_local_change(snapshot(), now);
        assert_eq!(offers(&wire), 0, "nothing to send to yet");

        sync.add_peer(2);
        sync.on_local_change(snapshot(), now);
        assert_eq!(offers(&wire), 1, "the pending copy must be offered");
    }

    /// Disabling and re-enabling clipboard sync must retry the same content.
    #[test]
    fn reset_allows_reoffering_the_same_content() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        let now = Instant::now();
        sync.on_local_change(snapshot(), now);
        assert_eq!(offers(&wire), 1);
        sync.reset();
        sync.on_local_change(snapshot(), now);
        assert_eq!(offers(&wire), 2);
    }

    /// Garbage from a misbehaving (but authenticated) peer must not panic or
    /// poison the sync state.
    #[test]
    fn hostile_frames_are_ignored_without_panicking() {
        use lan_mouse_proto::clipboard::MIME_TEXT;

        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        let now = Instant::now();
        // Ack without a transfer, impossible ack base, unknown chunk, cancel
        // and an offer that exceeds the limits.
        sync.handle_frame(
            2,
            ClipboardFrame::Ack {
                serial: 99,
                mime: MIME_TEXT,
                base: u32::MAX,
                bitmap: u32::MAX,
            },
            now,
        );
        sync.handle_frame(
            2,
            ClipboardFrame::Chunk {
                serial: 1,
                mime: MIME_TEXT,
                offset: u32::MAX,
                total: u32::MAX,
                data: vec![0; 4],
            },
            now,
        );
        sync.handle_frame(2, ClipboardFrame::Cancel { serial: 1234 }, now);
        sync.handle_frame(
            2,
            ClipboardFrame::Offer {
                serial: 1,
                origin: 7,
                hash: [0; 32],
                items: vec![(MIME_TEXT, u32::MAX)],
            },
            now,
        );
        sync.handle_frame(
            999,
            ClipboardFrame::Offer {
                serial: 2,
                origin: 7,
                hash: [0; 32],
                items: vec![(MIME_TEXT, 3)],
            },
            now,
        );
        assert!(sync.tick(now).is_none());
        assert!(
            sync.last_error().is_some(),
            "the oversized offer is reported"
        );
    }

    /// If a transfer dies (peer never acknowledges), copying the same content
    /// again must be retried instead of being treated as a duplicate.
    #[test]
    fn failed_transfer_allows_reoffering_the_same_content() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        let start = Instant::now();
        sync.on_local_change(snapshot(), start);
        assert_eq!(offers(&wire), 1);

        // Nobody answers: let the retransmissions exhaust themselves.
        let mut now = start;
        for _ in 0..2_000 {
            now += std::time::Duration::from_millis(10);
            sync.tick(now);
        }
        assert!(sync.last_error().is_some(), "the transfer should fail");

        wire.borrow_mut().clear();
        sync.on_local_change(snapshot(), now);
        assert_eq!(
            offers(&wire),
            1,
            "the same content must be retried after a failure"
        );
    }

    /// The service re-reads the clipboard and calls `on_local_change` when a
    /// peer registers. A previously offered hash must not swallow that
    /// re-offer after a reconnect, otherwise a copy interrupted by the
    /// disconnect is never retried (the reported first-Ctrl+C-lost symptom).
    #[test]
    fn reconnect_reoffers_the_current_clipboard_on_registration() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        let now = Instant::now();
        sync.on_local_change(snapshot(), now);
        assert_eq!(offers(&wire), 1);

        sync.remove_peer(2);
        wire.borrow_mut().clear();
        sync.add_peer(2);
        // The service reads the backend and offers the current clipboard.
        sync.on_local_change(snapshot(), now);
        assert_eq!(
            offers(&wire),
            1,
            "the reconnect resync was swallowed as a duplicate change event"
        );
    }

    /// `offer_current_to` must bypass the duplicate-change dedupe on purpose:
    /// the peer has just registered and has not seen the current clipboard.
    #[test]
    fn offer_current_to_bypasses_the_duplicate_dedupe() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        let now = Instant::now();
        sync.on_local_change(snapshot(), now);
        assert_eq!(offers(&wire), 1);
        wire.borrow_mut().clear();

        sync.offer_current_to(2, &snapshot(), now);
        assert_eq!(offers(&wire), 1, "the current clipboard must be offered");
    }

    #[test]
    fn offer_current_to_unknown_peer_is_ignored() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        sync.offer_current_to(99, &snapshot(), Instant::now());
        assert!(wire.borrow().is_empty());
    }

    /// A local copy after a remote apply invalidates the echo marker: copying
    /// the previously applied content again is a real copy and must sync.
    #[test]
    fn a_local_copy_invalidates_the_applied_echo_marker() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        let remote = snapshot();
        let now = Instant::now();
        sync.on_applied(&remote);

        // Echo of the applied content is suppressed.
        sync.on_local_change(remote.clone(), now);
        assert_eq!(offers(&wire), 0, "the applied content was echoed back");

        // A different local copy must be offered and clears the echo marker.
        let local = ClipboardSnapshot::from_items([(Format::Text, b"local copy".to_vec())]);
        sync.on_local_change(local, now);
        assert_eq!(offers(&wire), 1);

        wire.borrow_mut().clear();
        sync.on_local_change(remote, now);
        assert_eq!(
            offers(&wire),
            1,
            "re-copying previously applied content was swallowed as an echo"
        );
    }

    /// A peer that offers content we already applied (lost final ack, retried
    /// copy) must be acknowledged as complete instead of retransferred.
    #[test]
    fn an_offer_for_already_applied_content_is_acknowledged_as_complete() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        let copy = snapshot();
        sync.on_applied(&copy);

        sync.handle_frame(
            2,
            ClipboardFrame::Offer {
                serial: 77,
                origin: 0xfeed,
                hash: copy.hash(),
                items: copy.wire_items(),
            },
            Instant::now(),
        );

        let frames = wire.borrow();
        assert!(
            !frames
                .iter()
                .any(|(_, frame)| matches!(frame, ClipboardFrame::Chunk { .. })),
            "already applied content must not be retransferred"
        );
        let acks: Vec<&ClipboardFrame> = frames
            .iter()
            .map(|(_, frame)| frame)
            .filter(|frame| matches!(frame, ClipboardFrame::Ack { .. }))
            .collect();
        assert_eq!(acks.len(), copy.len(), "one completion ack per format");
        for ack in acks {
            if let ClipboardFrame::Ack { base, mime, .. } = ack {
                let expected = copy
                    .get(Format::from_id(*mime).expect("mime"))
                    .map(|data| (data.len() as u32).div_ceil(CHUNK_SIZE as u32))
                    .expect("format");
                assert_eq!(*base, expected, "the ack must mark every chunk received");
            }
        }
    }

    /// A peer cancelling a transfer must not swallow the next copy of the same
    /// content: the dedupe state is reset and the copy can be retried.
    #[test]
    fn a_peer_cancel_allows_reoffering_the_same_content() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        let now = Instant::now();
        sync.on_local_change(snapshot(), now);
        let serial = wire
            .borrow()
            .iter()
            .find_map(|(_, frame)| match frame {
                ClipboardFrame::Offer { serial, .. } => Some(*serial),
                _ => None,
            })
            .expect("offer");
        sync.handle_frame(2, ClipboardFrame::Cancel { serial }, now);
        assert!(sync.last_error().is_some(), "the cancellation is reported");

        wire.borrow_mut().clear();
        sync.on_local_change(snapshot(), now);
        assert_eq!(offers(&wire), 1, "the same content must be offerable again");
    }

    /// A failed transfer is re-offered automatically after a bounded delay, so
    /// a link outage does not require the user to press Ctrl+C again.
    #[test]
    fn a_failed_transfer_is_reoffered_automatically() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        let start = Instant::now();
        sync.on_local_change(snapshot(), start);
        assert_eq!(offers(&wire), 1);

        let mut now = start;
        for _ in 0..400 {
            now += std::time::Duration::from_millis(10);
            sync.tick(now);
        }
        assert!(sync.last_error().is_some(), "the transfer should fail");
        wire.borrow_mut().clear();

        let mut reoffered = false;
        for _ in 0..400 {
            now += std::time::Duration::from_millis(10);
            sync.tick(now);
            if offers(&wire) > 0 {
                reoffered = true;
                break;
            }
        }
        assert!(reoffered, "a failed copy must be re-offered automatically");
    }

    /// The final acknowledgement is withheld until the snapshot was actually
    /// written to the local clipboard: a write failure must not look delivered.
    #[test]
    fn the_final_ack_is_withheld_until_the_apply_commit() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        let now = Instant::now();
        let copy = ClipboardSnapshot::from_items([(Format::Text, b"single chunk".to_vec())]);
        let total = copy.get(Format::Text).expect("text").len() as u32;

        sync.handle_frame(
            2,
            ClipboardFrame::Offer {
                serial: 5,
                origin: 9,
                hash: copy.hash(),
                items: copy.wire_items(),
            },
            now,
        );
        sync.handle_frame(
            2,
            ClipboardFrame::Chunk {
                serial: 5,
                mime: MIME_TEXT,
                offset: 0,
                total,
                data: copy.get(Format::Text).expect("text").to_vec(),
            },
            now,
        );
        assert!(
            !wire
                .borrow()
                .iter()
                .any(|(_, frame)| matches!(frame, ClipboardFrame::Ack { .. })),
            "the final ack must wait for the clipboard commit"
        );
        let completed = sync.tick(now).expect("transfer completed");
        assert_eq!(completed, copy);
        assert!(
            !wire
                .borrow()
                .iter()
                .any(|(_, frame)| matches!(frame, ClipboardFrame::Ack { .. })),
            "no ack may be sent before on_applied"
        );

        sync.on_applied(&completed);
        assert!(
            wire.borrow()
                .iter()
                .any(|(_, frame)| matches!(frame, ClipboardFrame::Ack { .. })),
            "the withheld ack must be released after a successful apply"
        );
    }

    /// If the local clipboard write fails, the sender must be told to abort
    /// (it re-offers later) instead of receiving the final ack.
    #[test]
    fn a_failed_apply_cancels_the_sender_instead_of_acknowledging() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        let now = Instant::now();
        let copy = ClipboardSnapshot::from_items([(Format::Text, b"single chunk".to_vec())]);
        let total = copy.get(Format::Text).expect("text").len() as u32;
        sync.handle_frame(
            2,
            ClipboardFrame::Offer {
                serial: 6,
                origin: 9,
                hash: copy.hash(),
                items: copy.wire_items(),
            },
            now,
        );
        sync.handle_frame(
            2,
            ClipboardFrame::Chunk {
                serial: 6,
                mime: MIME_TEXT,
                offset: 0,
                total,
                data: copy.get(Format::Text).expect("text").to_vec(),
            },
            now,
        );
        let completed = sync.tick(now).expect("transfer completed");
        wire.borrow_mut().clear();
        sync.on_apply_failed(&completed);
        assert!(
            wire.borrow().iter().any(
                |(_, frame)| matches!(frame, ClipboardFrame::Cancel { serial } if *serial == 6)
            ),
            "a failed apply must cancel the sender's transfer"
        );
        assert!(
            !wire
                .borrow()
                .iter()
                .any(|(_, frame)| matches!(frame, ClipboardFrame::Ack { .. })),
            "a failed apply must not acknowledge the transfer"
        );
    }

    /// A serial collision from another sync epoch must not fake-acknowledge a
    /// stale transfer: same serial with a different origin/hash is a new
    /// transfer.
    #[test]
    fn a_same_serial_offer_from_another_epoch_replaces_the_transfer() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        let now = Instant::now();
        let first = ClipboardSnapshot::from_items([(Format::Text, b"first".to_vec())]);
        let second = ClipboardSnapshot::from_items([(Format::Text, b"second".to_vec())]);
        sync.handle_frame(
            2,
            ClipboardFrame::Offer {
                serial: 1,
                origin: 0x1111,
                hash: first.hash(),
                items: first.wire_items(),
            },
            now,
        );
        sync.handle_frame(
            2,
            ClipboardFrame::Offer {
                serial: 1,
                origin: 0x2222,
                hash: second.hash(),
                items: second.wire_items(),
            },
            now,
        );
        let data = second.get(Format::Text).expect("text").to_vec();
        sync.handle_frame(
            2,
            ClipboardFrame::Chunk {
                serial: 1,
                mime: MIME_TEXT,
                offset: 0,
                total: data.len() as u32,
                data,
            },
            now,
        );
        assert_eq!(
            sync.tick(now).expect("the second transfer completes"),
            second,
            "a stale same-serial transfer must not be treated as progress"
        );
    }

    /// Migrating an address peer to the outgoing client handle keeps an
    /// in-flight incoming transfer: the service must not tear it down when the
    /// duplicate peer identities are merged.
    #[test]
    fn migrate_peer_keeps_an_in_flight_incoming_transfer() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        let now = Instant::now();
        let payload = vec![0x5a; CHUNK_SIZE + 3];
        let copy = ClipboardSnapshot::from_items([(Format::Text, payload.clone())]);
        sync.handle_frame(
            2,
            ClipboardFrame::Offer {
                serial: 11,
                origin: 3,
                hash: copy.hash(),
                items: vec![(MIME_TEXT, payload.len() as u32)],
            },
            now,
        );
        sync.handle_frame(
            2,
            ClipboardFrame::Chunk {
                serial: 11,
                mime: MIME_TEXT,
                offset: 0,
                total: payload.len() as u32,
                data: payload[..CHUNK_SIZE].to_vec(),
            },
            now,
        );
        sync.migrate_peer(2, 7);
        sync.handle_frame(
            7,
            ClipboardFrame::Chunk {
                serial: 11,
                mime: MIME_TEXT,
                offset: CHUNK_SIZE as u32,
                total: payload.len() as u32,
                data: payload[CHUNK_SIZE..].to_vec(),
            },
            now,
        );
        assert_eq!(
            sync.tick(now).expect("the transfer survives the merge"),
            copy
        );
    }

    #[test]
    fn sync_instances_use_distinct_serial_epochs() {
        assert_ne!(
            serial_epoch(0xaaaa_0000_0000_0001),
            serial_epoch(0xaaaa_0000_0000_0002)
        );
        assert_ne!(
            serial_epoch(0x1111_2222_3333_4444),
            serial_epoch(0x1111_2222_3333_4445)
        );
    }

    /// A complete transfer queued before a disconnect must not be applied
    /// after it: a late datagram must not recreate the incoming transfer.
    #[test]
    fn a_transfer_queued_before_a_disconnect_is_not_applied_after_it() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        let now = Instant::now();
        let copy =
            ClipboardSnapshot::from_items([(Format::Text, b"queued before disconnect".to_vec())]);
        let data = copy.get(Format::Text).expect("text").to_vec();
        let frames = [
            ClipboardFrame::Offer {
                serial: 21,
                origin: 0x99,
                hash: copy.hash(),
                items: copy.wire_items(),
            },
            ClipboardFrame::Chunk {
                serial: 21,
                mime: MIME_TEXT,
                offset: 0,
                total: data.len() as u32,
                data,
            },
        ];

        sync.remove_peer(2);
        for frame in frames {
            sync.handle_frame(2, frame, now);
        }
        assert!(
            sync.tick(now).is_none(),
            "a queued frame from a disconnected peer recreated a transfer"
        );
        assert_eq!(sync.applied_count(), 0);
    }

    /// A write-only device (clipboard-write granted, clipboard-read denied) is
    /// still registered so its own content/acks are accepted, but our content
    /// is never offered to it.
    #[test]
    fn a_write_only_peer_can_send_content_but_is_not_offered_ours() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        sync.set_peer_may_receive(2, false);
        let now = Instant::now();
        let inbound =
            ClipboardSnapshot::from_items([(Format::Text, b"from write-only peer".to_vec())]);
        let data = inbound.get(Format::Text).expect("text").to_vec();
        sync.handle_frame(
            2,
            ClipboardFrame::Offer {
                serial: 31,
                origin: 0xaa,
                hash: inbound.hash(),
                items: inbound.wire_items(),
            },
            now,
        );
        sync.handle_frame(
            2,
            ClipboardFrame::Chunk {
                serial: 31,
                mime: MIME_TEXT,
                offset: 0,
                total: data.len() as u32,
                data,
            },
            now,
        );
        let applied = sync
            .tick(now)
            .expect("a write-only peer's content must still be accepted");
        assert_eq!(applied, inbound);
        sync.on_applied(&applied);
        wire.borrow_mut().clear();

        sync.on_local_change(
            ClipboardSnapshot::from_items([(Format::Text, b"our own copy".to_vec())]),
            now,
        );
        assert!(
            wire.borrow().is_empty(),
            "our clipboard must not be offered to a write-only peer"
        );
    }

    /// Applying newer remote content cancels an in-flight older local copy so
    /// it cannot overwrite the peer's newer clipboard after it arrives.
    #[test]
    fn applying_remote_content_cancels_superseded_in_flight_transfers() {
        let wire: Wire = Default::default();
        let mut sync = sync_with_peer(&wire);
        let now = Instant::now();
        let older = ClipboardSnapshot::from_items([(Format::Text, b"older local copy".to_vec())]);
        sync.on_local_change(older, now);
        let serial = wire
            .borrow()
            .iter()
            .find_map(|(_, frame)| match frame {
                ClipboardFrame::Offer { serial, .. } => Some(*serial),
                _ => None,
            })
            .expect("offer");
        wire.borrow_mut().clear();

        let newer = ClipboardSnapshot::from_items([(Format::Text, b"newer remote copy".to_vec())]);
        sync.on_applied(&newer);
        assert!(
            wire.borrow().iter().any(|(_, frame)| matches!(
                frame,
                ClipboardFrame::Cancel { serial: cancelled } if *cancelled == serial
            )),
            "the superseded in-flight transfer must be cancelled"
        );
    }
}
