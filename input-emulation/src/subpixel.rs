#![cfg_attr(not(any(windows, x11)), allow(dead_code))]

//! Sub-pixel accumulator for integer-only emulation backends.
//!
//! The wire format carries `f64` deltas. Wayland (libei/layer-shell) produces
//! fractional movements such as `0.4` per event; Windows `SendInput` and XTest
//! only accept integers, so a naive `as i32` truncates every event to zero and
//! the cursor appears frozen for slow movements. Keeping the remainder makes
//! the total motion exact.

/// Carries the fractional part between calls.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct SubPixel {
    rem_x: f64,
    rem_y: f64,
}

impl SubPixel {
    /// Add a fractional delta and return the integer part to apply now.
    ///
    /// Non-finite deltas (NaN or infinity) can arrive from a corrupted packet
    /// or a misbehaving peer. They must never enter the accumulator: a single
    /// NaN poisons the remainder for the rest of the session (`NaN - NaN` stays
    /// NaN) and makes all following movement disappear.
    pub(crate) fn consume(&mut self, dx: f64, dy: f64) -> (i32, i32) {
        let dx = if dx.is_finite() { dx } else { 0.0 };
        let dy = if dy.is_finite() { dy } else { 0.0 };
        let total_x = dx + self.rem_x;
        let total_y = dy + self.rem_y;
        let move_x = total_x.trunc();
        let move_y = total_y.trunc();
        self.rem_x = finite_remainder(total_x - move_x);
        self.rem_y = finite_remainder(total_y - move_y);
        (move_x as i32, move_y as i32)
    }
}

/// Clamp an accumulator remainder to its mathematical domain `(-1, 1)`.
///
/// The subtraction above is exact for every finite input, but an already
/// poisoned remainder (from an older version or a future arithmetic bug) would
/// stay poisoned forever, so re-sanitise it defensively.
fn finite_remainder(remainder: f64) -> f64 {
    if remainder.is_finite() && (-1.0..1.0).contains(&remainder) {
        remainder
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_fractional_moves_are_not_lost() {
        let mut acc = SubPixel::default();
        // Five 0.4px events must add up to two pixels, not zero.
        let mut total = 0;
        for _ in 0..5 {
            total += acc.consume(0.4, 0.0).0;
        }
        assert_eq!(total, 2);
    }

    #[test]
    fn integer_moves_pass_through() {
        let mut acc = SubPixel::default();
        assert_eq!(acc.consume(3.0, -2.0), (3, -2));
        assert_eq!(acc.consume(1.0, 1.0), (1, 1));
    }

    #[test]
    fn remainder_is_kept_between_calls() {
        let mut acc = SubPixel::default();
        assert_eq!(acc.consume(0.6, 0.0), (0, 0));
        assert_eq!(acc.consume(0.6, 0.0), (1, 0));
    }

    #[test]
    fn negative_fractions_accumulate_too() {
        let mut acc = SubPixel::default();
        assert_eq!(acc.consume(-0.6, 0.0), (0, 0));
        assert_eq!(acc.consume(-0.6, 0.0), (-1, 0));
    }

    #[test]
    fn total_motion_matches_the_sum_of_the_deltas() {
        let deltas = [0.3, -0.7, 1.2, 0.05, -0.85, 2.0];
        let mut acc = SubPixel::default();
        let applied: f64 = deltas.iter().map(|d| acc.consume(*d, 0.0).0 as f64).sum();
        let expected: f64 = deltas.iter().sum::<f64>().trunc();
        assert_eq!(applied, expected);
    }

    /// A corrupted packet can carry NaN/inf motion. It must be ignored instead
    /// of poisoning the remainder (which would freeze every following move).
    #[test]
    fn non_finite_deltas_do_not_poison_the_accumulator() {
        let mut acc = SubPixel::default();
        assert_eq!(acc.consume(f64::NAN, f64::INFINITY), (0, 0));
        assert_eq!(acc.consume(0.6, 0.0), (0, 0));
        assert_eq!(acc.consume(0.6, 0.0), (1, 0));

        assert_eq!(acc.consume(f64::NEG_INFINITY, f64::NAN), (0, 0));
        assert_eq!(acc.consume(0.6, 0.0), (0, 0));
        assert_eq!(acc.consume(0.6, 0.0), (1, 0));
    }

    /// Huge finite deltas saturate the integer delta (Rust float-to-int casts
    /// saturate) but must not leave a unusable remainder behind.
    #[test]
    fn huge_deltas_do_not_poison_the_accumulator() {
        let mut acc = SubPixel::default();
        assert_eq!(acc.consume(1e300, -1e300), (i32::MAX, i32::MIN));
        assert_eq!(acc.consume(0.6, 0.0), (0, 0));
        assert_eq!(acc.consume(0.6, 0.0), (1, 0));
    }
}
