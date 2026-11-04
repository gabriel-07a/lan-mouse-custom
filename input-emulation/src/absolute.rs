//! Coordinate maths for absolute pointer movement on Windows.
//!
//! `SendInput` with `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` expects
//! coordinates normalised to `0..=65535` over the whole virtual desktop
//! (all monitors). Keeping the arithmetic here makes it testable on any host.

/// Clamp `current + delta` to the virtual screen so the pointer cannot wrap.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn clamp_to_span(current: i32, delta: i32, origin: i32, span: i32) -> i32 {
    let span = span.max(1);
    (current.saturating_add(delta)).clamp(origin, origin + span)
}

/// Normalise an absolute coordinate to `0..=65535`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn normalise(value: i32, origin: i32, span: i32) -> i32 {
    let span = span.max(1);
    let offset = (value - origin).clamp(0, span) as i64;
    ((offset * 65535 + span as i64 / 2) / span as i64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_covers_the_full_range() {
        assert_eq!(normalise(0, 0, 1919), 0);
        assert_eq!(normalise(1919, 0, 1919), 65535);
        // Roughly the middle of a 1920px wide screen.
        let middle = normalise(960, 0, 1919);
        assert!((32_000..=33_500).contains(&middle), "middle was {middle}");
    }

    #[test]
    fn normalise_respects_a_multi_monitor_origin() {
        // Two screens side by side, the virtual desktop starting at -1920.
        assert_eq!(normalise(-1920, -1920, 3839), 0);
        assert_eq!(normalise(1919, -1920, 3839), 65535);
    }

    #[test]
    fn clamp_keeps_the_pointer_on_screen() {
        assert_eq!(clamp_to_span(100, -500, 0, 1919), 0);
        assert_eq!(clamp_to_span(100, 5000, 0, 1919), 1919);
        assert_eq!(clamp_to_span(100, 10, 0, 1919), 110);
    }

    #[test]
    fn clamp_handles_a_negative_origin() {
        assert_eq!(clamp_to_span(0, -50, -1920, 3839), -50);
    }

    #[test]
    fn zero_span_never_divides_by_zero() {
        assert_eq!(normalise(5, 0, 0), 65535);
        assert_eq!(clamp_to_span(5, 5, 0, 0), 1);
    }
}
