//! Opt-in structured tracing for the mouse hand-off.
//!
//! The normal debug log is too noisy to leave enabled while reproducing a
//! cursor stutter. `LAN_MOUSE_HANDOFF_TRACE=1` turns on a small number of
//! `target=handoff_trace` lines on both the controller and the receiver, so
//! the stages can be compared without flooding the log:
//!
//!   capture.begin -> capture.enter_sent -> capture.first_motion_sent
//!   receiver.enter_received -> receiver.armed -> receiver.ack_sent
//!   receiver.first_input -> receiver.input_gap_exceeded
//!
//! All lines include the peer address/serial where available.

use std::{sync::OnceLock, time::Duration};

/// A gap between two injected inputs above this value is worth a trace line.
pub(crate) const INPUT_GAP_WARN: Duration = Duration::from_millis(8);

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var("LAN_MOUSE_HANDOFF_TRACE").is_ok_and(|value| value == "1"))
}

/// Write one opt-in hand-off trace line.
pub(crate) fn trace(stage: &str, detail: impl std::fmt::Display) {
    if enabled() {
        log::info!(target: "handoff_trace", "{stage}: {detail}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_gap_threshold_is_eight_milliseconds() {
        assert!(!super::INPUT_GAP_WARN.is_zero());
        assert_eq!(super::INPUT_GAP_WARN, Duration::from_millis(8));
    }

    #[test]
    fn disabled_trace_is_a_noop() {
        // The environment flag is process-global and normally unset in tests.
        if !enabled() {
            trace("test.stage", format_args!("detail"));
        }
    }
}
