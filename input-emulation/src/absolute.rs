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
    (overshoot > 0).then_some(edge)
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
}
