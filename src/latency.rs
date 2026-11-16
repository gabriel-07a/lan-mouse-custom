//! Latency measurement for a peer link.
//!
//! Three independent measurements answer "where does the delay come from":
//!
//! - [`ProbeTracker`]: round-trip time of individual probe datagrams
//!   (`LatencyProbe`/`LatencyReply`), giving percentiles and loss for the
//!   network path itself;
//! - a send [`CadenceMeter`]: the interval between consecutive motion
//!   datagrams leaving this machine while the pointer is moving;
//! - a receive [`CadenceMeter`]: the interval between consecutive motion
//!   events arriving from the peer.
//!
//! A send cadence that is already irregular points at the sender (timers,
//! scheduling); a regular send cadence with an irregular receive cadence on the
//! other machine points at the network.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// Samples kept per measurement. At four probes per second this is one minute
/// of round-trip history; for motion cadence it is the last few seconds of
/// movement.
const WINDOW_SAMPLES: usize = 240;

/// A gap longer than this between two motion events is a pause of the hand,
/// not a delayed packet, and is not a cadence sample.
const MOVING_GAP_MAX: Duration = Duration::from_millis(100);

/// How long a probe may stay unanswered before it counts as lost.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// How often a probe is sent on an established link.
pub(crate) const PROBE_INTERVAL: Duration = Duration::from_millis(250);

/// How often the event loop is sampled for scheduling delay.
const LOOP_LAG_TICK: Duration = Duration::from_millis(10);

/// A wake-up later than this is reported: it is the latency budget of one
/// motion event, so anything above it is a visible stutter.
const LOOP_LAG_WARN: Duration = Duration::from_millis(10);

/// At most one report per this interval, so a busy period cannot flood the log.
const LOOP_LAG_REPORT_EVERY: Duration = Duration::from_secs(10);

/// Late wake-ups of the event loop over one reporting interval.
#[derive(Debug, Default)]
pub(crate) struct LoopLag {
    late: u32,
    worst: Duration,
}

impl LoopLag {
    /// Record one wake-up that was `lag` behind schedule.
    pub(crate) fn record(&mut self, lag: Duration) {
        if lag >= LOOP_LAG_WARN {
            self.late += 1;
            self.worst = self.worst.max(lag);
        }
    }

    /// Take the `(late wake-ups, worst delay)` of the interval, if any.
    pub(crate) fn take(&mut self) -> Option<(u32, Duration)> {
        let report = (self.late > 0).then_some((self.late, self.worst));
        *self = Self::default();
        report
    }
}

/// Watch how late the service's event loop wakes up.
///
/// Input, network, clipboard and IPC share one thread. Anything that holds it
/// (a large clipboard payload, synchronous file I/O) delays every motion event
/// behind it, and that delay is invisible in the network measurements. This
/// task only measures and logs; it never yields more than a timer tick.
pub(crate) async fn watch_loop_lag() {
    let mut lag = LoopLag::default();
    let mut last_report = Instant::now();
    loop {
        let scheduled = Instant::now() + LOOP_LAG_TICK;
        tokio::time::sleep_until(scheduled.into()).await;
        let now = Instant::now();
        lag.record(now.saturating_duration_since(scheduled));
        if now.saturating_duration_since(last_report) >= LOOP_LAG_REPORT_EVERY {
            last_report = now;
            if let Some((late, worst)) = lag.take() {
                log::warn!(
                    "event loop stalled {late} time(s) in the last {} s, worst {:.1} ms: \
                     input was delayed by other work on the service thread",
                    LOOP_LAG_REPORT_EVERY.as_secs(),
                    worst.as_secs_f64() * 1000.0
                );
            }
        }
    }
}

/// Fixed-size window of the most recent samples, in microseconds.
#[derive(Debug, Default)]
pub(crate) struct SampleWindow {
    samples: VecDeque<u32>,
}

impl SampleWindow {
    pub(crate) fn push(&mut self, micros: u32) {
        if self.samples.len() == WINDOW_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(micros);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.samples.len()
    }

    /// Nearest-rank percentile (`p` in 0.0..=1.0) of the window, or `None`
    /// while it is empty.
    pub(crate) fn percentile(&self, p: f64) -> Option<u32> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<u32> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        let rank = (p.clamp(0.0, 1.0) * sorted.len() as f64).ceil() as usize;
        Some(sorted[rank.clamp(1, sorted.len()) - 1])
    }
}

fn micros(duration: Duration) -> u32 {
    duration.as_micros().min(u128::from(u32::MAX)) as u32
}

/// Interval between consecutive motion events while the pointer is moving.
#[derive(Debug, Default)]
pub(crate) struct CadenceMeter {
    last: Option<Instant>,
    window: SampleWindow,
}

impl CadenceMeter {
    /// Record one motion event observed at `now`.
    pub(crate) fn record(&mut self, now: Instant) {
        if let Some(last) = self.last {
            let gap = now.saturating_duration_since(last);
            if gap <= MOVING_GAP_MAX {
                self.window.push(micros(gap));
            }
        }
        self.last = Some(now);
    }

    pub(crate) fn window(&self) -> &SampleWindow {
        &self.window
    }
}

/// Round-trip probes of one transport.
#[derive(Debug, Default)]
pub(crate) struct ProbeTracker {
    next_seq: u32,
    /// probes sent and not yet answered or expired, oldest first
    outstanding: VecDeque<(u32, Instant)>,
    rtt: SampleWindow,
    /// outcome of the most recent probes (`true` = lost), same window size
    outcomes: VecDeque<bool>,
}

impl ProbeTracker {
    /// Register a new probe sent at `now` and return its sequence number.
    /// Probes that outlived [`PROBE_TIMEOUT`] are counted as lost here.
    pub(crate) fn begin(&mut self, now: Instant) -> u32 {
        self.expire(now);
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.outstanding.push_back((seq, now));
        seq
    }

    /// A reply for `seq` arrived at `now`. Returns the round-trip time, or
    /// `None` for an unknown, duplicate or already expired probe.
    pub(crate) fn complete(&mut self, seq: u32, now: Instant) -> Option<Duration> {
        self.expire(now);
        let index = self.outstanding.iter().position(|(s, _)| *s == seq)?;
        let (_, sent) = self.outstanding.remove(index)?;
        let rtt = now.saturating_duration_since(sent);
        self.rtt.push(micros(rtt));
        self.record_outcome(false);
        Some(rtt)
    }

    fn expire(&mut self, now: Instant) {
        while let Some((_, sent)) = self.outstanding.front() {
            if now.saturating_duration_since(*sent) < PROBE_TIMEOUT {
                break;
            }
            self.outstanding.pop_front();
            self.record_outcome(true);
        }
    }

    fn record_outcome(&mut self, lost: bool) {
        if self.outcomes.len() == WINDOW_SAMPLES {
            self.outcomes.pop_front();
        }
        self.outcomes.push_back(lost);
    }

    pub(crate) fn rtt(&self) -> &SampleWindow {
        &self.rtt
    }

    /// Lost probes per thousand over the recent window, or `None` before any
    /// probe was resolved.
    pub(crate) fn loss_permille(&self) -> Option<u16> {
        if self.outcomes.is_empty() {
            return None;
        }
        let lost = self.outcomes.iter().filter(|lost| **lost).count();
        Some((lost * 1000 / self.outcomes.len()) as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn loop_lag_reports_only_stalls_and_resets() {
        let mut lag = LoopLag::default();
        lag.record(Duration::from_millis(1));
        assert_eq!(lag.take(), None);
        lag.record(Duration::from_millis(12));
        lag.record(Duration::from_millis(40));
        lag.record(Duration::from_millis(3));
        assert_eq!(lag.take(), Some((2, Duration::from_millis(40))));
        assert_eq!(lag.take(), None);
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        let mut window = SampleWindow::default();
        assert_eq!(window.percentile(0.5), None);
        for value in 1..=100 {
            window.push(value);
        }
        assert_eq!(window.percentile(0.5), Some(50));
        assert_eq!(window.percentile(0.99), Some(99));
        assert_eq!(window.percentile(1.0), Some(100));
        assert_eq!(window.percentile(0.0), Some(1));
    }

    #[test]
    fn window_keeps_only_the_most_recent_samples() {
        let mut window = SampleWindow::default();
        for value in 0..(WINDOW_SAMPLES as u32 + 10) {
            window.push(value);
        }
        assert_eq!(window.len(), WINDOW_SAMPLES);
        assert_eq!(window.percentile(0.0), Some(10));
    }

    #[test]
    fn cadence_ignores_pauses_of_the_hand() {
        let base = Instant::now();
        let mut meter = CadenceMeter::default();
        meter.record(base);
        meter.record(at(base, 2));
        meter.record(at(base, 4));
        // The hand stopped for half a second: not a delayed packet.
        meter.record(at(base, 504));
        meter.record(at(base, 520));
        assert_eq!(meter.window().len(), 3);
        assert_eq!(meter.window().percentile(1.0), Some(16_000));
        assert_eq!(meter.window().percentile(0.0), Some(2_000));
    }

    #[test]
    fn probe_round_trip_is_measured_per_sequence_number() {
        let base = Instant::now();
        let mut probes = ProbeTracker::default();
        let first = probes.begin(base);
        let second = probes.begin(at(base, 250));
        // Replies may arrive out of order.
        assert_eq!(
            probes.complete(second, at(base, 253)),
            Some(Duration::from_millis(3))
        );
        assert_eq!(
            probes.complete(first, at(base, 260)),
            Some(Duration::from_millis(260))
        );
        // A duplicate reply is not a second sample.
        assert_eq!(probes.complete(first, at(base, 261)), None);
        assert_eq!(probes.rtt().len(), 2);
        assert_eq!(probes.loss_permille(), Some(0));
    }

    #[test]
    fn unanswered_probes_count_as_lost() {
        let base = Instant::now();
        let mut probes = ProbeTracker::default();
        assert_eq!(probes.loss_permille(), None);
        let lost = probes.begin(base);
        let answered = probes.begin(at(base, 1100));
        assert!(probes.complete(answered, at(base, 1104)).is_some());
        assert_eq!(probes.loss_permille(), Some(500));
        // A reply that arrives after the timeout stays lost.
        assert_eq!(probes.complete(lost, at(base, 1200)), None);
        assert_eq!(probes.rtt().len(), 1);
    }
}
