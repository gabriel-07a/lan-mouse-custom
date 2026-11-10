//! Coordinate maths for absolute pointer movement on Windows.
//!
//! `SendInput` with `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` expects
//! coordinates normalised to `0..=65535` over the whole virtual desktop
//! (all monitors). The desktop bounds must be expressed in the *same*
//! coordinate space as `GetCursorPos`, otherwise the normalised fraction is
//! wrong.
//!
//! A per-monitor-DPI-aware thread sees physical pixels from both
//! `GetCursorPos` and `EnumDisplayMonitors`. `GetSystemMetrics`, however, is
//! documented as not DPI aware and keeps returning system-DPI (virtualised)
//! values: on a 125% display it reports 864 px of height where the cursor
//! coordinates go up to 1079. Everything below 864 then normalises to
//! 65535, which pins the pointer to the bottom edge of the screen.
//!
//! Keeping the arithmetic here makes it testable on any host.

use std::time::{Duration, Instant};

/// How often the return hand-off may be requested while the remote peer keeps
/// pushing past our screen edge.
pub(crate) const EDGE_RETURN_THROTTLE: Duration = Duration::from_millis(250);

/// How many hand-off requests one contiguous edge contact may send. A single
/// `Leave` can be lost over UDP, hence the retries; but an unbounded retry
/// stream would keep shoving control back to a peer that ignores it.
pub(crate) const EDGE_RETURN_MAX_ATTEMPTS: u8 = 3;

/// Bounding rectangle of the virtual desktop, in cursor coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VirtualScreen {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}

impl VirtualScreen {
    /// Bounding box of the given monitor rectangles
    /// `(x, y, width, height)`.
    ///
    /// Returns `None` when no usable monitor is present, in which case the
    /// caller must fall back to relative movement instead of guessing.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn from_monitors(monitors: &[(i32, i32, i32, i32)]) -> Option<Self> {
        let mut usable = monitors.iter().filter(|&&(_, _, w, h)| w > 0 && h > 0);
        let &(x, y, w, h) = usable.next()?;
        let (mut left, mut top) = (x, y);
        let (mut right, mut bottom) = (x.saturating_add(w), y.saturating_add(h));

        for &(x, y, w, h) in usable {
            left = left.min(x);
            top = top.min(y);
            right = right.max(x.saturating_add(w));
            bottom = bottom.max(y.saturating_add(h));
        }

        Some(Self {
            left,
            top,
            width: right - left,
            height: bottom - top,
        })
    }

    fn right(&self) -> i32 {
        self.left.saturating_add(self.width - 1)
    }

    fn bottom(&self) -> i32 {
        self.top.saturating_add(self.height - 1)
    }

    /// Whether `point` lies inside the virtual desktop. Used to detect that
    /// the cursor and the enumerated monitors disagree (DPI change, monitor
    /// hot-plug) before normalising anything.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn contains(&self, (x, y): (i32, i32)) -> bool {
        (self.left..=self.right()).contains(&x) && (self.top..=self.bottom()).contains(&y)
    }

    /// Clamp a point to the virtual desktop so the pointer cannot wrap.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn clamp(&self, (x, y): (i32, i32)) -> (i32, i32) {
        (
            x.clamp(self.left, self.right()),
            y.clamp(self.top, self.bottom()),
        )
    }

    /// Normalise a point to `0..=65535` over the virtual desktop.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn normalise(&self, (x, y): (i32, i32)) -> (i32, i32) {
        (
            normalise(x, self.left, self.width - 1),
            normalise(y, self.top, self.height - 1),
        )
    }
}

/// Normalise an absolute coordinate to `0..=65535`.
fn normalise(value: i32, origin: i32, span: i32) -> i32 {
    let span = span.max(1);
    let offset = (value - origin).clamp(0, span) as i64;
    ((offset * 65535 + span as i64 / 2) / span as i64) as i32
}

/// A usable DPI scale factor, falling back to 100% for nonsense input.
fn sanitise_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

/// Convert a logical (96 DPI) delta from the wire to the physical pixels used
/// by pointer emulation. The capture side performs the inverse conversion
/// (physical / scale) before putting a delta on the wire.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn logical_to_physical(delta: f64, scale: f64) -> f64 {
    delta * sanitise_scale(scale)
}

/// Minimum overshoot, in physical pixels, before a target counts as pushed
/// past a screen edge.
///
/// A single pixel is produced by pointer jitter at the border and used to
/// bounce control between the machines; a deliberate push still crosses it
/// immediately.
pub(crate) const EDGE_RETURN_MIN_OVERSHOOT: i32 = 2;

/// How far inside the entry edge the cursor is parked when remote control
/// starts.
///
/// Mirrors the libei absolute tracker's anchor. Without it the Windows cursor
/// keeps whatever position it had before the peer entered: parked exactly on
/// the edge it hands control back on the first pixel of jitter, parked
/// elsewhere the first deltas clamp at the wrong edge (the cursor looks frozen
/// until the user pulls the other way).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const ENTRY_ANCHOR_MARGIN: i32 = 24;

/// Screen edge that an unclamped pointer target overshoots.
///
/// The emulation backend computes the target as `cursor + requested motion`
/// and Windows clamps the actual pointer to the virtual desktop. When the
/// remote peer keeps pushing outward at an edge, the unclamped target crosses
/// it — that is the return hand-off: the peer must release its capture so the
/// cursor can come back.
///
/// When both axes overflow (corner), the larger overshoot wins.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn overshoot_edge(
    screen: &VirtualScreen,
    requested: (i32, i32),
) -> Option<super::PointerEdge> {
    let (x, y) = requested;
    let mut candidates = [
        (
            super::PointerEdge::Left,
            screen.left.saturating_sub(x).max(0),
        ),
        (
            super::PointerEdge::Right,
            x.saturating_sub(screen.right()).max(0),
        ),
        (super::PointerEdge::Top, screen.top.saturating_sub(y).max(0)),
        (
            super::PointerEdge::Bottom,
            y.saturating_sub(screen.bottom()).max(0),
        ),
    ];
    candidates.sort_by_key(|&(_, overshoot)| std::cmp::Reverse(overshoot));
    let (edge, overshoot) = candidates[0];
    (overshoot >= EDGE_RETURN_MIN_OVERSHOOT).then_some(edge)
}

/// Park the cursor `margin` pixels inside `edge`, keeping the coordinate along
/// the edge at `along` (clamped to the screen).
///
/// Used when remote control starts so the cursor has a deterministic entry
/// point: small movements back towards the edge do not immediately count as a
/// return hand-off, and the first injected deltas cannot clamp at another edge.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn entry_anchor(
    screen: &VirtualScreen,
    edge: super::PointerEdge,
    along: i32,
    margin: i32,
) -> (i32, i32) {
    // Keep the anchor inside the screen even for a tiny virtual desktop.
    let margin_x = margin.clamp(1, (screen.width / 2).max(1));
    let margin_y = margin.clamp(1, (screen.height / 2).max(1));
    match edge {
        super::PointerEdge::Left => (
            screen.left.saturating_add(margin_x),
            along.clamp(screen.top, screen.bottom()),
        ),
        super::PointerEdge::Right => (
            screen.right().saturating_sub(margin_x),
            along.clamp(screen.top, screen.bottom()),
        ),
        super::PointerEdge::Top => (
            along.clamp(screen.left, screen.right()),
            screen.top.saturating_add(margin_y),
        ),
        super::PointerEdge::Bottom => (
            along.clamp(screen.left, screen.right()),
            screen.bottom().saturating_sub(margin_y),
        ),
    }
}

/// Throttled, bounded retry latch for the return hand-off requests.
///
/// The Windows backend asks the peer to release its capture whenever the
/// remote pointer is pushed past a screen edge. One `Leave` can be lost over
/// UDP, so the request is retried a few times — but only a bounded number of
/// times per contiguous edge contact. Without the latch, a peer that keeps
/// pushing at the edge would produce an unbounded stream of `Leave` packets
/// and could bounce the capture back and forth between the two machines.
#[derive(Debug, Default)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct EdgeReturnLatch {
    /// The edge currently being requested.
    edge: Option<super::PointerEdge>,
    /// Hand-off requests sent for this contiguous edge contact.
    attempts: u8,
    /// When the last request was sent.
    last: Option<Instant>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl EdgeReturnLatch {
    /// Re-arm the latch: the remote pointer is back inside the screen (or the
    /// session ended), so the next edge contact is a new hand-off.
    pub(crate) fn clear(&mut self) {
        self.edge = None;
        self.attempts = 0;
        self.last = None;
    }

    /// Ask for one more hand-off request for `edge`, if it is due.
    ///
    /// Returns the 1-based attempt number to log when a request should be
    /// sent, `None` when it is throttled or the retry budget for this edge
    /// contact is exhausted.
    pub(crate) fn request(&mut self, edge: super::PointerEdge, now: Instant) -> Option<u8> {
        let attempts = if self.edge == Some(edge) {
            self.attempts
        } else {
            0
        };
        if attempts >= EDGE_RETURN_MAX_ATTEMPTS {
            return None;
        }
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < EDGE_RETURN_THROTTLE)
        {
            return None;
        }
        let attempt = attempts + 1;
        self.edge = Some(edge);
        self.attempts = attempt;
        self.last = Some(now);
        Some(attempt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> VirtualScreen {
        VirtualScreen::from_monitors(&[(0, 0, 1920, 1080)]).expect("screen")
    }

    #[test]
    fn overshoot_detects_every_edge() {
        assert_eq!(
            overshoot_edge(&screen(), (-3, 500)),
            Some(super::super::PointerEdge::Left)
        );
        assert_eq!(
            overshoot_edge(&screen(), (1925, 500)),
            Some(super::super::PointerEdge::Right)
        );
        assert_eq!(
            overshoot_edge(&screen(), (500, (-2))),
            Some(super::super::PointerEdge::Top)
        );
        assert_eq!(
            overshoot_edge(&screen(), (500, 1085)),
            Some(super::super::PointerEdge::Bottom)
        );
    }

    #[test]
    fn inside_targets_do_not_hand_off() {
        assert_eq!(overshoot_edge(&screen(), (0, 0)), None);
        assert_eq!(overshoot_edge(&screen(), (1919, 1079)), None);
        assert_eq!(overshoot_edge(&screen(), (960, 540)), None);
    }

    #[test]
    fn jitter_below_the_minimum_overshoot_is_ignored() {
        // One pixel past the edge is pointer jitter, not a deliberate push.
        assert_eq!(overshoot_edge(&screen(), (-1, 500)), None);
        assert_eq!(overshoot_edge(&screen(), (1920, 500)), None);
        assert_eq!(overshoot_edge(&screen(), (500, (-1))), None);
        assert_eq!(overshoot_edge(&screen(), (500, 1080)), None);
        // From the threshold on it is a real hand-off.
        assert_eq!(
            overshoot_edge(&screen(), (-2, 500)),
            Some(super::super::PointerEdge::Left)
        );
        assert_eq!(
            overshoot_edge(&screen(), (1921, 500)),
            Some(super::super::PointerEdge::Right)
        );
    }

    #[test]
    fn entry_anchor_parks_inside_each_edge() {
        let screen = screen();
        let margin = ENTRY_ANCHOR_MARGIN;
        assert_eq!(
            entry_anchor(&screen, super::super::PointerEdge::Left, 500, margin),
            (margin, 500)
        );
        assert_eq!(
            entry_anchor(&screen, super::super::PointerEdge::Right, 500, margin),
            (1919 - margin, 500)
        );
        assert_eq!(
            entry_anchor(&screen, super::super::PointerEdge::Top, 500, margin),
            (500, margin)
        );
        assert_eq!(
            entry_anchor(&screen, super::super::PointerEdge::Bottom, 500, margin),
            (500, 1079 - margin)
        );
    }

    #[test]
    fn entry_anchor_clamps_along_and_stays_inside_a_tiny_screen() {
        let screen = screen();
        assert_eq!(
            entry_anchor(
                &screen,
                super::super::PointerEdge::Left,
                -100,
                ENTRY_ANCHOR_MARGIN
            ),
            (ENTRY_ANCHOR_MARGIN, 0)
        );
        assert_eq!(
            entry_anchor(
                &screen,
                super::super::PointerEdge::Left,
                5000,
                ENTRY_ANCHOR_MARGIN
            ),
            (ENTRY_ANCHOR_MARGIN, 1079)
        );
        let tiny = VirtualScreen::from_monitors(&[(0, 0, 10, 10)]).expect("tiny screen");
        let (x, y) = entry_anchor(
            &tiny,
            super::super::PointerEdge::Right,
            5,
            ENTRY_ANCHOR_MARGIN,
        );
        assert!(
            (0..10).contains(&x) && (0..10).contains(&y),
            "anchor ({x},{y}) escaped the tiny screen"
        );
    }

    #[test]
    fn a_corner_prefers_the_larger_overshoot() {
        assert_eq!(
            overshoot_edge(&screen(), (-5, (-2))),
            Some(super::super::PointerEdge::Left)
        );
        assert_eq!(
            overshoot_edge(&screen(), (-1, 1090)),
            Some(super::super::PointerEdge::Bottom)
        );
    }

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
    fn zero_span_never_divides_by_zero() {
        assert_eq!(normalise(5, 0, 0), 65535);
    }

    #[test]
    fn single_monitor_covers_the_screen() {
        let screen = VirtualScreen::from_monitors(&[(0, 0, 1920, 1080)]).unwrap();
        assert_eq!(
            (screen.left, screen.top, screen.width, screen.height),
            (0, 0, 1920, 1080)
        );
        assert_eq!(screen.normalise((0, 0)), (0, 0));
        assert_eq!(screen.normalise((1919, 1079)), (65535, 65535));
        assert!(screen.contains((1919, 1079)));
        assert!(!screen.contains((1920, 1080)));
    }

    /// Regression: on a 125% display the old code took the height from
    /// `GetSystemMetrics` (864, virtualised) although `GetCursorPos` returns
    /// physical pixels (up to 1079). A cursor at y=1000 was clamped to 863
    /// and normalised to 65535, gluing it to the bottom of the screen.
    #[test]
    fn scaled_display_is_not_pinned_to_the_bottom() {
        let physical = VirtualScreen::from_monitors(&[(0, 0, 1920, 1080)]).unwrap();
        let (_, y) = physical.normalise((960, 1000));
        assert_ne!(
            y, 65535,
            "cursor at y=1000 must not be clamped to the bottom"
        );
        assert!(y > 60_000, "y was {y}");
    }

    #[test]
    fn multi_monitor_bounds_span_negative_origins() {
        // A monitor above and to the left of the primary.
        let screen =
            VirtualScreen::from_monitors(&[(-1920, -1080, 1920, 1080), (0, 0, 1920, 1080)])
                .unwrap();
        assert_eq!(
            (screen.left, screen.top, screen.width, screen.height),
            (-1920, -1080, 3840, 2160)
        );
        assert_eq!(screen.clamp((-5000, 5000)), (-1920, 1079));
        assert_eq!(screen.normalise((-1920, -1080)), (0, 0));
        assert_eq!(screen.normalise((1919, 1079)), (65535, 65535));
    }

    #[test]
    fn monitor_rectangles_not_on_an_axis_aligned_grid_are_unioned() {
        // L-shaped layout: the second monitor is below the first one.
        let screen =
            VirtualScreen::from_monitors(&[(0, 0, 1920, 1080), (0, 1080, 1280, 1024)]).unwrap();
        assert_eq!((screen.width, screen.height), (1920, 2104));
        assert_eq!(screen.clamp((500, 5000)), (500, 2103));
    }

    #[test]
    fn no_usable_monitors_returns_none() {
        assert_eq!(VirtualScreen::from_monitors(&[]), None);
        assert_eq!(VirtualScreen::from_monitors(&[(0, 0, 0, 1080)]), None);
    }

    /// The wire carries logical pixels: 125 physical px on a 125% sender are
    /// 100 logical px, which are 150 physical px on a 150% receiver. Both
    /// machines therefore move the cursor the same logical distance.
    #[test]
    fn motion_keeps_its_logical_distance_across_scales() {
        let sent = 125.0 / 1.25; // capture: physical -> logical
        assert_eq!(sent, 100.0);
        assert_eq!(logical_to_physical(sent, 1.5), 150.0);
    }

    #[test]
    fn conversion_round_trips_at_the_same_scale() {
        for delta in [-100.0, -1.5, 0.0, 0.5, 42.0] {
            let logical = delta / 1.25; // capture: physical -> logical
            let physical = logical_to_physical(logical, 1.25);
            assert!(
                (physical - delta).abs() < f64::EPSILON,
                "{physical} != {delta}"
            );
        }
    }

    /// A 100% sender feeding a 150% receiver must move 1.5x the physical
    /// pixels to keep the same logical speed.
    #[test]
    fn higher_receiver_scale_moves_more_physical_pixels() {
        assert_eq!(logical_to_physical(10.0, 1.5), 15.0);
        assert_eq!(logical_to_physical(10.0, 2.0), 20.0);
        assert_eq!(15.0 / 1.5, 10.0); // capture: physical -> logical
    }

    #[test]
    fn nonsense_scale_falls_back_to_100_percent() {
        assert_eq!(logical_to_physical(10.0, 0.0), 10.0);
        assert_eq!(logical_to_physical(10.0, -1.0), 10.0);
        assert_eq!(logical_to_physical(10.0, f64::NAN), 10.0);
    }

    fn edge() -> super::super::PointerEdge {
        super::super::PointerEdge::Left
    }

    #[test]
    fn edge_return_requests_are_throttled_and_bounded() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut latch = EdgeReturnLatch::default();

        assert_eq!(latch.request(edge(), t0), Some(1));
        assert_eq!(
            latch.request(edge(), t0 + Duration::from_millis(100)),
            None,
            "throttled while a request is in flight"
        );
        assert_eq!(
            latch.request(edge(), t0 + EDGE_RETURN_THROTTLE),
            Some(2),
            "one retry per throttle interval"
        );
        assert_eq!(
            latch.request(edge(), t0 + EDGE_RETURN_THROTTLE * 2),
            Some(3)
        );
        assert_eq!(
            latch.request(edge(), t0 + EDGE_RETURN_THROTTLE * 3),
            None,
            "retry budget of one edge contact is exhausted"
        );
        assert_eq!(
            latch.request(edge(), t0 + EDGE_RETURN_THROTTLE * 100),
            None,
            "a peer that keeps pushing must not generate more Leave packets"
        );
    }

    #[test]
    fn edge_return_latch_rearms_after_the_pointer_returns_inside() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut latch = EdgeReturnLatch::default();
        for attempt in 0..EDGE_RETURN_MAX_ATTEMPTS {
            let now = t0 + EDGE_RETURN_THROTTLE * u32::from(attempt);
            assert_eq!(latch.request(edge(), now), Some(attempt + 1));
        }
        assert!(
            latch
                .request(edge(), t0 + EDGE_RETURN_THROTTLE * 100)
                .is_none()
        );
        latch.clear();
        assert_eq!(
            latch.request(edge(), t0 + Duration::from_millis(1)),
            Some(1),
            "a new edge contact starts a new hand-off"
        );
    }

    #[test]
    fn edge_return_latch_restarts_its_budget_for_a_different_edge() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut latch = EdgeReturnLatch::default();
        assert_eq!(latch.request(edge(), t0), Some(1));
        assert_eq!(
            latch.request(super::super::PointerEdge::Bottom, t0),
            None,
            "still throttled across the edge change"
        );
        assert_eq!(
            latch.request(
                super::super::PointerEdge::Bottom,
                t0 + Duration::from_millis(300)
            ),
            Some(1),
            "the new edge gets its own retry budget"
        );
    }
}
