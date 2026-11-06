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
    pub(crate) fn consume(&mut self, dx: f64, dy: f64) -> (i32, i32) {
        let total_x = dx + self.rem_x;
        let total_y = dy + self.rem_y;
        let move_x = total_x.trunc();
        let move_y = total_y.trunc();
        self.rem_x = total_x - move_x;
        self.rem_y = total_y - move_y;
        (move_x as i32, move_y as i32)
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
}
