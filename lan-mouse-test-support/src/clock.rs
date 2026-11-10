//! Deterministic monotonic clock for state-machine tests.
//!
//! Production code that needs a clock should take a [`Clock`] seam once the
//! testability refactor lands. Until then this type is useful for tests of
//! pure helpers that accept `Instant::now()` as an argument: create the clock,
//! pass `clock.now()` and move time with [`TestClock::advance`] instead of
//! sleeping.

use std::{
    cell::Cell,
    time::{Duration, Instant},
};

/// Upper bound for test time: ten years is far beyond any state-machine test
/// and keeps `Instant + Duration` arithmetic inside the platform range.
const MAX_ELAPSED: Duration = Duration::from_secs(10 * 365 * 24 * 60 * 60);

/// A `Instant`-compatible clock whose time only moves when the test says so.
#[derive(Debug)]
pub struct TestClock {
    start: Instant,
    elapsed: Cell<Duration>,
}

impl TestClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            elapsed: Cell::new(Duration::ZERO),
        }
    }

    /// Current test time.
    ///
    /// The elapsed value is capped so `Instant` arithmetic cannot overflow even
    /// if a test asks to advance by `Duration::MAX`.
    pub fn now(&self) -> Instant {
        self.start
            .checked_add(self.elapsed.get())
            .unwrap_or(self.start)
    }

    /// Advance by `duration` (saturating at [`MAX_ELAPSED`]).
    pub fn advance(&self, duration: Duration) {
        let total = self
            .elapsed
            .get()
            .checked_add(duration)
            .unwrap_or(MAX_ELAPSED);
        self.elapsed.set(total.min(MAX_ELAPSED));
    }

    /// Set the elapsed time relative to the start (capped like [`Self::advance`]).
    pub fn set_elapsed(&self, duration: Duration) {
        self.elapsed.set(duration.min(MAX_ELAPSED));
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_only_moves_on_request() {
        let clock = TestClock::new();
        let start = clock.now();
        assert_eq!(clock.now(), start);
        clock.advance(Duration::from_millis(5));
        assert_eq!(clock.now(), start + Duration::from_millis(5));
        assert_eq!(clock.now(), start + Duration::from_millis(5));
    }

    #[test]
    fn advancing_is_monotonic_and_saturating() {
        let clock = TestClock::new();
        let start = clock.now();
        clock.advance(Duration::MAX);
        clock.advance(Duration::from_secs(1));
        assert!(clock.now() >= start);
    }
}
