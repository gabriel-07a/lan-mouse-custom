//! Reliable-ish clipboard transfer over an unreliable datagram transport.
//!
//! DTLS gives us authenticated, ordered-but-lossy datagrams. A snapshot is
//! split into [`CHUNK_SIZE`] chunks; the sender keeps a window of
//! [`ACK_WINDOW`] chunks in flight and retransmits the ones the receiver did
//! not acknowledge. Retransmission stops after
//! [`MAX_WINDOW_ATTEMPTS`] windows without progress, so a dead peer cannot
//! stall a transfer forever.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use lan_mouse_proto::clipboard::{ACK_WINDOW, CHUNK_SIZE, ClipboardFrame};

use crate::{ClipboardSnapshot, Error, Format, Limits};

/// How long the sender waits for an acknowledgement before resending the
/// current window.
pub const ACK_TIMEOUT: Duration = Duration::from_millis(120);
/// Abort after this many windows without any progress.
///
/// The retransmission delay grows with every unanswered window (see
/// [`retransmission_delay`]), so this is several seconds of tolerance for a
/// slow link or a short Wi-Fi hiccup while still failing well before a user
/// would conclude the transfer hung. The sync layer additionally re-offers a
/// failed copy, see `ClipboardSync`'s pending retry.
pub const MAX_WINDOW_ATTEMPTS: u8 = 6;
/// Maximum number of chunks emitted in a single [`OutgoingTransfer::poll`]
/// burst so bulk transfers do not flood the low-latency UDP socket at once.
pub const MAX_CHUNKS_PER_BURST: usize = 6;
/// Send an acknowledgement at least this often while chunks keep arriving.
pub const ACK_EVERY_CHUNKS: u32 = 16;
pub const ACK_EVERY: Duration = Duration::from_millis(10);
/// Drop an incomplete incoming transfer after this long.
pub const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);

fn chunk_count(total: u32) -> u32 {
    total.div_ceil(CHUNK_SIZE as u32)
}

/// Wait before retransmitting a window that has not made progress for
/// `attempts` windows.
///
/// The first couple of retries are fast (an isolated lost datagram is cheap to
/// repair), then the delay doubles so a congested Wi-Fi link, a scheduling
/// stall or a peer that is briefly busy does not exhaust the retry budget in
/// little more than a second.
pub fn retransmission_delay(attempts: u8) -> Duration {
    let factor = match attempts {
        0..=2 => 1,
        3..=4 => 2,
        _ => 4,
    };
    ACK_TIMEOUT * factor
}

/// Sender side of one snapshot to one peer.
#[derive(Debug)]
pub struct OutgoingTransfer {
    serial: u32,
    origin: u64,
    hash: [u8; 32],
    items: Vec<OutgoingItem>,
    offer_sent: bool,
    acked_any: bool,
    current: usize,
    failed: bool,
}

#[derive(Debug)]
struct OutgoingItem {
    format: Format,
    data: Vec<u8>,
    total_chunks: u32,
    acked: Vec<bool>,
    base: u32,
    /// Next chunk index within the current window `[base, window_end)` to be
    /// emitted on the next paced burst.
    cursor: u32,
    attempts: u8,
    last_send: Option<Instant>,
}

impl OutgoingTransfer {
    pub fn new(serial: u32, origin: u64, snapshot: &ClipboardSnapshot) -> Self {
        let items = snapshot
            .iter()
            .map(|(format, data)| OutgoingItem {
                format,
                total_chunks: chunk_count(data.len() as u32),
                acked: vec![false; chunk_count(data.len() as u32) as usize],
                data: data.to_vec(),
                base: 0,
                cursor: 0,
                attempts: 0,
                last_send: None,
            })
            .collect();
        Self {
            serial,
            origin,
            hash: snapshot.hash(),
            items,
            offer_sent: false,
            acked_any: false,
            current: 0,
            failed: false,
        }
    }

    pub fn serial(&self) -> u32 {
        self.serial
    }

    /// Hash of the snapshot this transfer carries.
    pub fn hash(&self) -> [u8; 32] {
        self.hash
    }

    /// The snapshot this transfer carries, for re-offering after a failure.
    pub fn snapshot(&self) -> ClipboardSnapshot {
        let mut snapshot = ClipboardSnapshot::new();
        for item in &self.items {
            snapshot.insert(item.format, item.data.clone());
        }
        snapshot
    }

    fn offer_frame(&self) -> ClipboardFrame {
        ClipboardFrame::Offer {
            serial: self.serial,
            origin: self.origin,
            hash: self.hash,
            items: self
                .items
                .iter()
                .map(|item| (item.format.id(), item.data.len() as u32))
                .collect(),
        }
    }

    /// Frames that should be sent now (offer and/or a paced slice of the current window).
    pub fn poll(&mut self, now: Instant) -> Vec<ClipboardFrame> {
        let mut out = Vec::new();

        while self.current < self.items.len()
            && self.items[self.current].base >= self.items[self.current].total_chunks
        {
            self.current += 1;
        }
        if self.current >= self.items.len() || self.failed {
            return out;
        }

        let window_end =
            (self.items[self.current].base + ACK_WINDOW).min(self.items[self.current].total_chunks);
        let window_sweep_pending = self.items[self.current].cursor < window_end;
        let retransmission_due = self.items[self.current].last_send.is_none_or(|last| {
            now.duration_since(last) >= retransmission_delay(self.items[self.current].attempts)
        });

        // The offer is re-sent with every retransmission round, not only until
        // the first acknowledgement. A receiver that lost its transfer state
        // (clipboard sync toggled off/on, clipboard backend restarted) ignores
        // retransmitted chunks until it sees the offer again; without this the
        // sender could only give up and the user's copy would be lost.
        // Every peer build that speaks the clipboard extension already handles
        // a repeated offer by re-acknowledging the in-flight transfer, so this
        // needs no capability negotiation or wire change.
        if (!self.acked_any && !self.offer_sent) || (!window_sweep_pending && retransmission_due) {
            out.push(self.offer_frame());
            self.offer_sent = true;
        }

        let item = &mut self.items[self.current];
        if !window_sweep_pending {
            if !retransmission_due {
                return out;
            }
            item.cursor = item.base;
        }

        let mut sent_in_burst = 0usize;
        while item.cursor < window_end && sent_in_burst < MAX_CHUNKS_PER_BURST {
            let index = item.cursor;
            item.cursor += 1;
            if item.acked[index as usize] {
                continue;
            }
            let start = index as usize * CHUNK_SIZE;
            let end = (start + CHUNK_SIZE).min(item.data.len());
            out.push(ClipboardFrame::Chunk {
                serial: self.serial,
                mime: item.format.id(),
                offset: start as u32,
                total: item.data.len() as u32,
                data: item.data[start..end].to_vec(),
            });
            sent_in_burst += 1;
        }

        // Advance `cursor` past any trailing already-acked chunks in the window
        // so we can immediately arm the retransmission timer once all unacked
        // chunks in the window have been emitted.
        while item.cursor < window_end && item.acked[item.cursor as usize] {
            item.cursor += 1;
        }

        if item.cursor >= window_end {
            item.last_send = Some(now);
            item.attempts = item.attempts.saturating_add(1);
            if item.attempts > MAX_WINDOW_ATTEMPTS {
                log::warn!(
                    "clipboard transfer {}: no progress for {} attempts, aborting",
                    self.serial,
                    item.attempts
                );
                self.failed = true;
            }
        }
        out
    }

    /// Apply an acknowledgement. Returns whether it made progress.
    pub fn on_ack(&mut self, serial: u32, mime: u8, base: u32, bitmap: u32) -> bool {
        if serial != self.serial {
            return false;
        }
        let Some(format) = Format::from_id(mime) else {
            return false;
        };
        let Some(item) = self.items.iter_mut().find(|item| item.format == format) else {
            return false;
        };
        if base < item.base {
            return false; // stale ack for a window we already advanced past
        }
        // A cumulative count can never exceed the number of chunks. Rejecting
        // impossible values also keeps `base + bit` below from overflowing on
        // a hostile or corrupted acknowledgement.
        if base > item.total_chunks {
            return false;
        }

        // `base` is cumulative: every chunk below it arrived, even when the
        // bitmap is zero (the common case for a single-chunk representation).
        let mut progress = false;
        for index in item.base..base.min(item.total_chunks) {
            if !item.acked[index as usize] {
                item.acked[index as usize] = true;
                progress = true;
            }
        }
        for bit in 0..ACK_WINDOW {
            if bitmap & (1 << bit) == 0 {
                continue;
            }
            let Some(index) = base.checked_add(bit) else {
                continue;
            };
            if index < item.total_chunks && !item.acked[index as usize] {
                item.acked[index as usize] = true;
                progress = true;
            }
        }

        if progress {
            while item.base < item.total_chunks && item.acked[item.base as usize] {
                item.base += 1;
            }
            item.cursor = item.cursor.max(item.base);
            item.attempts = 0;
            item.last_send = None;
            self.acked_any = true;
        }
        progress
    }

    pub fn is_done(&self) -> bool {
        self.items.iter().all(|item| item.base >= item.total_chunks)
    }

    pub fn is_failed(&self) -> bool {
        self.failed
    }
}

/// Receiver side of one incoming snapshot.
#[derive(Debug)]
pub struct IncomingTransfer {
    serial: u32,
    origin: u64,
    hash: [u8; 32],
    items: BTreeMap<Format, IncomingItem>,
    started: Instant,
    last_update: Instant,
}

#[derive(Debug)]
struct IncomingItem {
    total: u32,
    data: Vec<u8>,
    received: Vec<bool>,
    chunks_since_ack: u32,
    last_ack: Instant,
}

impl IncomingTransfer {
    /// Create a receiver for an offer, enforcing the size limits.
    pub fn from_offer(
        serial: u32,
        origin: u64,
        hash: [u8; 32],
        items: &[(u8, u32)],
        limits: &Limits,
        now: Instant,
    ) -> Result<Self, Error> {
        if items.is_empty() {
            return Err(Error::Transfer("empty clipboard offer".into()));
        }
        let mut map = BTreeMap::new();
        let mut total = 0usize;
        for (mime, size) in items {
            let format = Format::from_id(*mime)
                .ok_or_else(|| Error::Transfer(format!("unknown mime id {mime}")))?;
            let size = *size as usize;
            if size > limits.max_for(format) {
                return Err(Error::TooLarge);
            }
            total = total.saturating_add(size);
            if total > limits.max_snapshot {
                return Err(Error::TooLarge);
            }
            map.insert(
                format,
                IncomingItem {
                    total: size as u32,
                    data: vec![0; size],
                    received: vec![false; chunk_count(size as u32) as usize],
                    chunks_since_ack: 0,
                    last_ack: now,
                },
            );
        }
        Ok(Self {
            serial,
            origin,
            hash,
            items: map,
            started: now,
            last_update: now,
        })
    }

    pub fn serial(&self) -> u32 {
        self.serial
    }

    pub fn origin(&self) -> u64 {
        self.origin
    }

    pub fn hash(&self) -> [u8; 32] {
        self.hash
    }

    /// Store a chunk. Returns an acknowledgement when one is due.
    pub fn on_chunk(
        &mut self,
        serial: u32,
        mime: u8,
        offset: u32,
        total: u32,
        data: &[u8],
        now: Instant,
    ) -> Result<Option<ClipboardFrame>, Error> {
        if serial != self.serial {
            return Ok(None);
        }
        let Some(format) = Format::from_id(mime) else {
            return Ok(None);
        };
        let Some(item) = self.items.get_mut(&format) else {
            return Ok(None);
        };
        if total != item.total || !offset.is_multiple_of(CHUNK_SIZE as u32) || data.is_empty() {
            return Ok(None);
        }
        if data.len() > CHUNK_SIZE {
            return Err(Error::Transfer("oversized clipboard chunk".into()));
        }
        let start = offset as usize;
        let end = start + data.len();
        if end > item.data.len() {
            return Err(Error::Transfer("clipboard chunk out of bounds".into()));
        }
        item.data[start..end].copy_from_slice(data);

        let first = offset / CHUNK_SIZE as u32;
        let last = (offset + data.len() as u32 - 1) / CHUNK_SIZE as u32;
        for index in first..=last {
            if let Some(received) = item.received.get_mut(index as usize) {
                *received = true;
            }
        }
        item.chunks_since_ack += 1;
        self.last_update = now;

        let complete = item.received.iter().all(|received| *received);
        if complete
            || item.chunks_since_ack >= ACK_EVERY_CHUNKS
            || now.duration_since(item.last_ack) >= ACK_EVERY
        {
            item.chunks_since_ack = 0;
            item.last_ack = now;
            let (base, bitmap) = window(item);
            return Ok(Some(ClipboardFrame::Ack {
                serial,
                mime,
                base,
                bitmap,
            }));
        }
        Ok(None)
    }

    pub fn is_complete(&self) -> bool {
        self.items
            .values()
            .all(|item| item.received.iter().all(|received| *received))
    }

    /// Current acknowledgements for every representation. Used when a
    /// retransmitted offer arrives for a transfer that is already in flight.
    pub fn acks(&self) -> Vec<ClipboardFrame> {
        self.items
            .iter()
            .map(|(format, item)| {
                let (base, bitmap) = window(item);
                ClipboardFrame::Ack {
                    serial: self.serial,
                    mime: format.id(),
                    base,
                    bitmap,
                }
            })
            .collect()
    }

    /// Rebuild the snapshot once every chunk arrived and verify its hash.
    pub fn assemble(&self) -> Result<ClipboardSnapshot, Error> {
        let mut snapshot = ClipboardSnapshot::new();
        for (format, item) in &self.items {
            snapshot.insert(*format, item.data.clone());
        }
        if snapshot.hash() != self.hash {
            return Err(Error::Transfer("clipboard hash mismatch".into()));
        }
        Ok(snapshot)
    }

    pub fn timed_out(&self, now: Instant) -> bool {
        now.duration_since(self.last_update) > TRANSFER_TIMEOUT
            && now.duration_since(self.started) > TRANSFER_TIMEOUT
    }
}

fn window(item: &IncomingItem) -> (u32, u32) {
    let base = item
        .received
        .iter()
        .position(|received| !received)
        .unwrap_or(item.received.len()) as u32;
    let mut bitmap = 0u32;
    for bit in 0..ACK_WINDOW {
        let index = base + bit;
        if item.received.get(index as usize).copied().unwrap_or(false) {
            bitmap |= 1 << bit;
        }
    }
    (base, bitmap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClipboardSnapshot;
    use lan_mouse_proto::clipboard::{MIME_PNG, MIME_TEXT};

    fn snapshot() -> ClipboardSnapshot {
        ClipboardSnapshot::from_items([
            (Format::Text, b"hello world".to_vec()),
            (Format::Html, b"<b>hello</b>".to_vec()),
            (Format::Png, vec![0x89; CHUNK_SIZE * 2 + 7]),
        ])
    }

    /// Drive a transfer over a link that drops frames selected by `should_drop`.
    fn drive(
        snapshot: &ClipboardSnapshot,
        mut should_drop: impl FnMut(&ClipboardFrame) -> bool,
    ) -> Result<ClipboardSnapshot, Error> {
        let t0 = Instant::now();
        let mut sender = OutgoingTransfer::new(1, 42, snapshot);
        let mut receiver: Option<IncomingTransfer> = None;
        let mut now = t0;
        let mut frames = sender.poll(now);
        let mut guard = 0;

        loop {
            guard += 1;
            assert!(guard < 10_000, "transfer did not converge");
            let mut acks = Vec::new();
            for frame in frames.drain(..) {
                if should_drop(&frame) {
                    continue;
                }
                match frame {
                    ClipboardFrame::Offer {
                        serial,
                        origin,
                        hash,
                        items,
                    } => {
                        // A repeated offer for the transfer already in flight
                        // must not reset the receiver (production re-acks it).
                        let repeated = receiver.as_ref().is_some_and(|current| {
                            current.serial() == serial
                                && current.origin() == origin
                                && current.hash() == hash
                        });
                        if !repeated {
                            receiver = Some(
                                IncomingTransfer::from_offer(
                                    serial,
                                    origin,
                                    hash,
                                    &items,
                                    &Limits::default(),
                                    now,
                                )
                                .expect("accept offer"),
                            );
                        }
                    }
                    ClipboardFrame::Chunk {
                        serial,
                        mime,
                        offset,
                        total,
                        data,
                    } => {
                        if let Some(receiver) = receiver.as_mut() {
                            if let Some(ack) = receiver
                                .on_chunk(serial, mime, offset, total, &data, now)
                                .expect("chunk")
                            {
                                acks.push(ack);
                            }
                        }
                    }
                    _ => {}
                }
            }

            if let Some(receiver) = receiver.as_ref() {
                // force a final acknowledgement for whatever arrived
                for format in receiver.items.keys() {
                    let item = &receiver.items[format];
                    let (base, bitmap) = window(item);
                    acks.push(ClipboardFrame::Ack {
                        serial: receiver.serial,
                        mime: format.id(),
                        base,
                        bitmap,
                    });
                }
            }

            for ack in acks {
                if let ClipboardFrame::Ack {
                    serial,
                    mime,
                    base,
                    bitmap,
                } = ack
                {
                    sender.on_ack(serial, mime, base, bitmap);
                }
            }

            if let Some(receiver) = receiver.as_ref() {
                if receiver.is_complete() {
                    return receiver.assemble();
                }
            }

            now += Duration::from_millis(10);
            frames = sender.poll(now);
            if sender.is_failed() {
                return Err(Error::Transfer("sender gave up".into()));
            }
        }
    }

    #[test]
    fn transfers_all_formats_without_loss() {
        let source = snapshot();
        let result = drive(&source, |_| false).expect("transfer");
        assert_eq!(result, source);
    }

    #[test]
    fn transfers_with_heavy_chunk_loss() {
        let source = snapshot();
        let mut seed = 0x1234_5678u64;
        let result = drive(&source, |_| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            seed % 100 < 25
        })
        .expect("transfer with loss");
        assert_eq!(result, source);
    }

    #[test]
    fn transfer_aborts_when_nothing_is_acknowledged() {
        let source = snapshot();
        let mut guard = 0;
        let result = drive(&source, |frame| {
            guard += 1;
            !matches!(frame, ClipboardFrame::Offer { .. })
        });
        assert!(result.is_err());
        assert!(guard > 1);
    }

    #[test]
    fn sender_ignores_stale_and_foreign_acks() {
        let source = snapshot();
        let mut sender = OutgoingTransfer::new(9, 0, &source);
        assert!(!sender.on_ack(8, MIME_TEXT, 0, u32::MAX), "foreign serial");
        assert!(!sender.on_ack(9, 200, 0, u32::MAX), "unknown mime");
        assert!(!sender.on_ack(9, MIME_TEXT, 0, 0), "no bits set");
    }

    #[test]
    fn sender_rejects_impossible_ack_bases_without_panicking() {
        let source = snapshot();
        let mut sender = OutgoingTransfer::new(9, 0, &source);
        // A corrupted/hostile ack must not overflow `base + bit` (debug
        // panic), must not fake progress and must not mark the transfer done.
        assert!(!sender.on_ack(9, MIME_TEXT, u32::MAX, u32::MAX));
        assert!(!sender.on_ack(9, MIME_TEXT, u32::MAX - 1, u32::MAX));
        assert!(!sender.is_done());
        // The highest valid cumulative base is accepted.
        let text_chunks = (b"hello world".len() as u32).div_ceil(CHUNK_SIZE as u32);
        assert!(sender.on_ack(9, MIME_TEXT, text_chunks, 0));
    }

    #[test]
    fn receiver_rejects_oversized_offers() {
        let limits = Limits {
            max_text: 10,
            ..Default::default()
        };
        let result = IncomingTransfer::from_offer(
            1,
            0,
            [0; 32],
            &[(MIME_TEXT, 11)],
            &limits,
            Instant::now(),
        );
        assert!(matches!(result, Err(Error::TooLarge)));
    }

    #[test]
    fn receiver_rejects_an_empty_offer() {
        assert!(
            IncomingTransfer::from_offer(1, 0, [0; 32], &[], &Limits::default(), Instant::now())
                .is_err()
        );
    }

    #[test]
    fn receiver_ignores_malformed_chunks() {
        let source = ClipboardSnapshot::from_items([(Format::Text, b"abc".to_vec())]);
        let mut receiver = IncomingTransfer::from_offer(
            1,
            0,
            source.hash(),
            &source.wire_items(),
            &Limits::default(),
            Instant::now(),
        )
        .expect("offer");
        // Offsets must be chunk aligned and totals must match the offer.
        assert!(
            receiver
                .on_chunk(1, MIME_TEXT, 5, 3, b"abc", Instant::now())
                .expect("ignored")
                .is_none()
        );
        assert!(
            receiver
                .on_chunk(1, MIME_TEXT, 0, 99, b"abc", Instant::now())
                .expect("ignored")
                .is_none()
        );
        // A chunk larger than the wire limit is rejected, not written.
        let oversized = vec![0u8; CHUNK_SIZE + 1];
        assert!(
            receiver
                .on_chunk(1, MIME_TEXT, 0, 3, &oversized, Instant::now())
                .is_err()
        );
        assert!(!receiver.is_complete());
    }

    #[test]
    fn hash_mismatch_is_detected() {
        let source = snapshot();
        let t0 = Instant::now();
        let mut receiver = IncomingTransfer::from_offer(
            1,
            0,
            [0xaa; 32],
            &source.wire_items(),
            &Limits::default(),
            t0,
        )
        .expect("offer");
        for (format, data) in source.iter() {
            let mut offset = 0usize;
            while offset < data.len() {
                let end = (offset + CHUNK_SIZE).min(data.len());
                receiver
                    .on_chunk(
                        1,
                        format.id(),
                        offset as u32,
                        data.len() as u32,
                        &data[offset..end],
                        t0,
                    )
                    .expect("chunk");
                offset = end;
            }
        }
        assert!(receiver.is_complete());
        assert!(matches!(receiver.assemble(), Err(Error::Transfer(_))));
    }

    #[test]
    fn large_window_is_paced_across_multiple_polls() {
        let large = ClipboardSnapshot::from_items([(Format::Png, vec![0x42; CHUNK_SIZE * 20])]);
        let mut sender = OutgoingTransfer::new(7, 1, &large);
        let t0 = Instant::now();

        let first = sender.poll(t0);
        let first_chunks = first
            .iter()
            .filter(|f| matches!(f, ClipboardFrame::Chunk { .. }))
            .count();
        assert_eq!(first_chunks, MAX_CHUNKS_PER_BURST);

        let second = sender.poll(t0 + Duration::from_millis(15));
        let second_chunks = second
            .iter()
            .filter(|f| matches!(f, ClipboardFrame::Chunk { .. }))
            .count();
        assert_eq!(second_chunks, MAX_CHUNKS_PER_BURST);
        assert!(
            !second
                .iter()
                .any(|f| matches!(f, ClipboardFrame::Offer { .. })),
            "offer should not be resent before ACK_TIMEOUT while sweeping the initial window"
        );
    }

    #[test]
    fn retransmission_backoff_grows_but_stays_bounded() {
        assert_eq!(retransmission_delay(1), ACK_TIMEOUT);
        assert_eq!(retransmission_delay(2), ACK_TIMEOUT);
        assert_eq!(retransmission_delay(3), ACK_TIMEOUT * 2);
        assert_eq!(retransmission_delay(4), ACK_TIMEOUT * 2);
        assert_eq!(retransmission_delay(5), ACK_TIMEOUT * 4);
        assert_eq!(retransmission_delay(MAX_WINDOW_ATTEMPTS), ACK_TIMEOUT * 4);
        // Every retransmission round is slower than the previous one or equal,
        // and the whole budget is a few seconds rather than a few hundred ms.
        let total: Duration = (1..=MAX_WINDOW_ATTEMPTS).map(retransmission_delay).sum();
        assert!(
            total >= Duration::from_millis(1_500),
            "retry budget too small for a Wi-Fi hiccup: {total:?}"
        );
        assert!(
            total <= Duration::from_secs(3),
            "retry budget too large to surface a failed transfer: {total:?}"
        );
    }

    /// The offer must be repeated in every retransmission round, also after
    /// the peer acknowledged part of the transfer: a receiver that lost its
    /// transfer state (sync reset or backend restart) only rebuilds it from
    /// the offer, and would otherwise ignore the retransmitted chunks forever.
    #[test]
    fn offer_is_repeated_with_every_retransmission_round() {
        let payload = ClipboardSnapshot::from_items([(Format::Png, vec![0x5a; CHUNK_SIZE * 40])]);
        let mut sender = OutgoingTransfer::new(3, 0, &payload);
        let t0 = Instant::now();
        let first = sender.poll(t0);
        assert!(
            first
                .iter()
                .any(|frame| matches!(frame, ClipboardFrame::Offer { .. })),
            "the first burst must carry the offer"
        );
        // The peer acknowledges the first burst: the transfer now has progress
        // (`acked_any` is set) and the offer used to stop being retransmitted.
        assert!(sender.on_ack(3, MIME_PNG, MAX_CHUNKS_PER_BURST as u32, 0));

        let mut now = t0;
        let mut last_chunk_at = t0;
        for _ in 0..20 {
            now += Duration::from_millis(10);
            let frames = sender.poll(now);
            if frames
                .iter()
                .any(|frame| matches!(frame, ClipboardFrame::Chunk { .. }))
            {
                last_chunk_at = now;
            } else {
                break;
            }
        }

        let retransmission = sender.poll(last_chunk_at + retransmission_delay(2));
        assert!(
            retransmission
                .iter()
                .any(|frame| matches!(frame, ClipboardFrame::Offer { .. })),
            "a retransmission round after progress must repeat the offer"
        );
        assert!(
            retransmission
                .iter()
                .any(|frame| matches!(frame, ClipboardFrame::Chunk { .. })),
            "the same round must also retransmit the unacknowledged window"
        );
    }

    /// A stalled transfer is retried for a few seconds (so a short link hiccup
    /// is absorbed instead of losing the copy) but still fails in bounded time.
    #[test]
    fn stalled_transfer_fails_after_the_backoff_budget_without_hanging() {
        let source = ClipboardSnapshot::from_items([(Format::Text, b"stalled".to_vec())]);
        let mut sender = OutgoingTransfer::new(5, 0, &source);
        let t0 = Instant::now();
        let mut now = t0;
        sender.poll(now);
        let mut failed_at = None;
        for _ in 0..500 {
            now += Duration::from_millis(10);
            sender.poll(now);
            if sender.is_failed() {
                failed_at = Some(now.duration_since(t0));
                break;
            }
        }
        let failed_at = failed_at.expect("a silent peer must not stall a transfer forever");
        assert!(
            failed_at >= Duration::from_millis(1_500),
            "gave up before a Wi-Fi hiccup could recover: {failed_at:?}"
        );
        assert!(
            failed_at <= Duration::from_secs(3),
            "took too long to surface a failed transfer: {failed_at:?}"
        );
    }

    #[test]
    fn snapshot_accessor_returns_the_transferred_content() {
        let source = snapshot();
        let sender = OutgoingTransfer::new(1, 0, &source);
        assert_eq!(sender.snapshot(), source);
        assert_eq!(sender.snapshot().hash(), source.hash());
    }
}
