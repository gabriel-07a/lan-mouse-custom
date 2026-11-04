//! Coalescing of pointer motion events.
//!
//! Sending one DTLS datagram per motion event is expensive on Wi-Fi:
//! a 1000 Hz mouse produces 1000 small packets per second, each with
//! its own DTLS record, IP and 802.11 overhead. Accumulating the
//! deltas for a few milliseconds and sending a single combined motion
//! event cuts the packet rate by an order of magnitude without a
//! perceptible latency cost (wireless mice already add several ms).
//!
//! Only [`PointerEvent::Motion`] is coalesced. Discrete events
//! (buttons, scroll, keyboard) must always be flushed in order and are
//! never dropped; callers are responsible for flushing the pending
//! motion of a handle before sending a discrete event.
//!
//! A `batch_ms` of `0` disables coalescing entirely.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use input_capture::CaptureHandle;

/// A coalesced pointer motion: the sum of the deltas and the time of the
/// most recent event.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Motion {
    pub(crate) time: u32,
    pub(crate) dx: f64,
    pub(crate) dy: f64,
}

#[derive(Debug)]
pub(crate) struct MotionCoalescer {
    batch: Option<Duration>,
    pending: HashMap<CaptureHandle, Motion>,
    last_flush: Instant,
    events_in: u64,
    batches_out: u64,
}

impl MotionCoalescer {
    pub(crate) fn new(batch_ms: u64) -> Self {
        Self {
            batch: (batch_ms > 0).then(|| Duration::from_millis(batch_ms)),
            pending: HashMap::new(),
            last_flush: Instant::now(),
            events_in: 0,
            batches_out: 0,
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.batch.is_some()
    }

    /// The batch window, or `None` when coalescing is disabled.
    pub(crate) fn interval(&self) -> Option<Duration> {
        self.batch
    }

    /// Accumulate a motion event for `handle`.
    ///
    /// Returns `true` when the batch window has elapsed and the caller
    /// should flush now.
    pub(crate) fn push(
        &mut self,
        handle: CaptureHandle,
        time: u32,
        dx: f64,
        dy: f64,
        now: Instant,
    ) -> bool {
        self.events_in += 1;

        match self.pending.get_mut(&handle) {
            Some(motion) => {
                motion.time = time;
                motion.dx += dx;
                motion.dy += dy;
            }
            None => {
                self.pending.insert(handle, Motion { time, dx, dy });
            }
        }

        match self.batch {
            Some(batch) => now.duration_since(self.last_flush) >= batch,
            None => false,
        }
    }

    /// Take the pending motion of a single handle (used before sending a
    /// discrete event for that handle).
    pub(crate) fn take(&mut self, handle: CaptureHandle) -> Option<Motion> {
        let motion = self.pending.remove(&handle);
        if motion.is_some() {
            self.last_flush = Instant::now();
            self.batches_out += 1;
        }
        motion
    }

    /// Drain every pending motion (used by the flush timer and on release).
    pub(crate) fn take_all(&mut self) -> Vec<(CaptureHandle, Motion)> {
        self.last_flush = Instant::now();
        let pending = self.pending.drain().collect::<Vec<_>>();
        self.batches_out += pending.len() as u64;
        pending
    }

    /// `(motion events received, motion packets sent)` since startup.
    pub(crate) fn counters(&self) -> (u64, u64) {
        (self.events_in, self.batches_out)
    }

    #[cfg(test)]
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc(dx: f64, dy: f64) -> Motion {
        Motion { time: 0, dx, dy }
    }

    #[test]
    fn disabled_when_batch_is_zero() {
        let mut c = MotionCoalescer::new(0);
        assert!(!c.enabled());
        assert!(!c.push(1, 0, 1.0, 1.0, Instant::now()));
        // still accumulates, caller decides what to do
        assert!(c.has_pending());
    }

    #[test]
    fn accumulates_deltas_and_keeps_latest_time() {
        let mut c = MotionCoalescer::new(4);
        let t0 = Instant::now();
        assert!(!c.push(1, 10, 1.0, 2.0, t0));
        assert!(!c.push(1, 11, 3.0, -1.0, t0));
        let motion = c.take(1).expect("pending motion");
        assert_eq!(
            motion,
            Motion {
                time: 11,
                dx: 4.0,
                dy: 1.0
            }
        );
        assert!(!c.has_pending());
    }

    #[test]
    fn handles_are_isolated() {
        let mut c = MotionCoalescer::new(4);
        let t0 = Instant::now();
        c.push(1, 0, 1.0, 0.0, t0);
        c.push(2, 0, 0.0, 1.0, t0);
        assert_eq!(c.take(1), Some(acc(1.0, 0.0)));
        assert_eq!(c.take(2), Some(acc(0.0, 1.0)));
    }

    #[test]
    fn due_after_batch_window() {
        let mut c = MotionCoalescer::new(4);
        let t0 = Instant::now();
        assert!(!c.push(1, 0, 1.0, 0.0, t0));
        assert!(!c.push(1, 0, 1.0, 0.0, t0 + Duration::from_millis(3)));
        assert!(c.push(1, 0, 1.0, 0.0, t0 + Duration::from_millis(5)));
    }

    #[test]
    fn take_all_drains_everything() {
        let mut c = MotionCoalescer::new(4);
        let t0 = Instant::now();
        c.push(1, 0, 1.0, 0.0, t0);
        c.push(2, 0, 0.0, 2.0, t0);
        let mut all = c.take_all();
        all.sort_by_key(|(h, _)| *h);
        assert_eq!(all, vec![(1, acc(1.0, 0.0)), (2, acc(0.0, 2.0))]);
        assert!(!c.has_pending());
    }

    #[test]
    fn counters_track_compression_ratio() {
        let mut c = MotionCoalescer::new(4);
        let t0 = Instant::now();
        for _ in 0..10 {
            c.push(1, 0, 1.0, 0.0, t0);
        }
        assert_eq!(c.counters(), (10, 0));
        assert!(c.take(1).is_some());
        assert_eq!(c.counters(), (10, 1));
    }

    #[test]
    fn take_after_flush_updates_window() {
        let mut c = MotionCoalescer::new(60_000);
        let t0 = Instant::now();
        c.push(1, 0, 1.0, 0.0, t0);
        assert!(c.take(1).is_some());
        // a fresh batch starts after the flush
        assert!(!c.push(1, 0, 1.0, 0.0, Instant::now()));
    }
}

#[cfg(test)]
mod extra_tests {
    use super::*;

    #[test]
    fn interval_reflects_configuration() {
        assert_eq!(MotionCoalescer::new(0).interval(), None);
        assert!(!MotionCoalescer::new(0).enabled());
        assert_eq!(
            MotionCoalescer::new(7).interval(),
            Some(Duration::from_millis(7))
        );
        assert!(MotionCoalescer::new(7).enabled());
    }

    #[test]
    fn take_all_counts_each_flushed_packet() {
        let mut c = MotionCoalescer::new(4);
        let t0 = Instant::now();
        c.push(1, 0, 1.0, 0.0, t0);
        c.push(2, 0, 0.0, 1.0, t0);
        c.push(3, 0, 0.0, 0.0, t0);
        assert_eq!(c.take_all().len(), 3);
        assert_eq!(c.counters(), (3, 3));
    }

    #[test]
    fn negative_and_zero_deltas_accumulate() {
        let mut c = MotionCoalescer::new(4);
        let t0 = Instant::now();
        c.push(1, 1, -2.5, 0.0, t0);
        c.push(1, 2, 1.5, -1.0, t0);
        c.push(1, 3, 0.0, 2.0, t0);
        let motion = c.take(1).expect("motion");
        assert_eq!(motion.dx, -1.0);
        assert_eq!(motion.dy, 1.0);
        assert_eq!(motion.time, 3);
    }

    #[test]
    fn disabled_mode_still_returns_what_was_accumulated() {
        let mut c = MotionCoalescer::new(0);
        let t0 = Instant::now();
        c.push(5, 9, 3.0, 4.0, t0);
        let motion = c.take(5).expect("motion");
        assert_eq!(motion.dx, 3.0);
        assert_eq!(motion.dy, 4.0);
        assert_eq!(c.counters(), (1, 1));
    }

    #[test]
    fn independent_handles_flush_in_a_single_drain() {
        let mut c = MotionCoalescer::new(4);
        let t0 = Instant::now();
        for handle in 0..4 {
            c.push(handle, handle as u32, 1.0, 1.0, t0);
        }
        let mut drained = c.take_all();
        drained.sort_by_key(|(handle, _)| *handle);
        assert_eq!(
            drained
                .iter()
                .map(|(handle, _)| *handle)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(c.counters(), (4, 4));
    }
}
