#[cfg(windows)]
use windows::Win32::Foundation::RECT;

#[cfg(not(windows))]
#[allow(clippy::upper_case_acronyms)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RECT {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

use crate::Position;

fn is_within_dp_region(point: (i32, i32), display: &RECT) -> bool {
    [
        Position::Left,
        Position::Right,
        Position::Top,
        Position::Bottom,
    ]
    .iter()
    .all(|&pos| is_within_dp_boundary(point, display, pos))
}

fn is_within_dp_boundary(point: (i32, i32), display: &RECT, pos: Position) -> bool {
    let (x, y) = point;
    match pos {
        Position::Left => display.left <= x,
        Position::Right => display.right > x,
        Position::Top => display.top <= y,
        Position::Bottom => display.bottom > y,
    }
}

/// returns whether the given position is within the display bounds with respect to the given
/// barrier position
///
/// # Arguments
///
/// * `x`:
/// * `y`:
/// * `displays`:
/// * `pos`:
///
/// returns: bool
///
fn in_bounds(point: (i32, i32), displays: &[RECT], pos: Position) -> bool {
    displays
        .iter()
        .any(|d| is_within_dp_boundary(point, d, pos))
}

fn in_display_region(point: (i32, i32), displays: &[RECT]) -> bool {
    displays.iter().any(|d| is_within_dp_region(point, d))
}

fn moved_across_boundary(
    prev_pos: (i32, i32),
    curr_pos: (i32, i32),
    displays: &[RECT],
    pos: Position,
) -> bool {
    /* was within bounds, but is not anymore */
    in_display_region(prev_pos, displays) && !in_bounds(curr_pos, displays, pos)
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn entered_barrier(
    prev_pos: (i32, i32),
    curr_pos: (i32, i32),
    displays: &[RECT],
) -> Option<Position> {
    [
        Position::Left,
        Position::Right,
        Position::Top,
        Position::Bottom,
    ]
    .into_iter()
    .find(|&pos| moved_across_boundary(prev_pos, curr_pos, displays, pos))
}

/// Bounding box of all display rectangles: `(left, top, right, bottom)`, with
/// `right`/`bottom` exclusive like `RECT`.
fn desktop_bounds(displays: &[RECT]) -> Option<(i32, i32, i32, i32)> {
    let mut iter = displays
        .iter()
        .filter(|d| d.right > d.left && d.bottom > d.top);
    let first = iter.next()?;
    let (mut left, mut top) = (first.left, first.top);
    let (mut right, mut bottom) = (first.right, first.bottom);
    for d in iter {
        left = left.min(d.left);
        top = top.min(d.top);
        right = right.max(d.right);
        bottom = bottom.max(d.bottom);
    }
    Some((left, top, right, bottom))
}

/// Detect a barrier crossing when the cursor has already been clamped by
/// Windows to the outer edge of the desktop.
///
/// This is the case the old out-of-bounds check missed: on the desktop edge
/// Windows stops the pointer at `right - 1` (or `left`, `top`, `bottom - 1`)
/// and `MSLLHOOKSTRUCT.pt` may never leave the monitor bounds, so
/// [`entered_barrier`] never fires and the cursor appears to stick to the
/// edge. Raw mouse input still reports the movement that pushed against the
/// edge, so "cursor is on the outermost pixel AND raw delta points outward"
/// is a reliable crossing signal regardless of how the OS clamps the pointer
/// (Windows 10 vs. 11, DPI virtualisation, ...).
///
/// `delta` is the raw mouse delta since the previous report, in mouse counts
/// (roughly physical pixels at the default pointer speed).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn outward_edge_crossing(
    cursor: (i32, i32),
    delta: (i32, i32),
    displays: &[RECT],
) -> Option<Position> {
    let (left, top, right, bottom) = desktop_bounds(displays)?;
    let (x, y) = cursor;
    [
        (Position::Left, x <= left && delta.0 < 0),
        (Position::Right, x >= right - 1 && delta.0 > 0),
        (Position::Top, y <= top && delta.1 < 0),
        (Position::Bottom, y >= bottom - 1 && delta.1 > 0),
    ]
    .into_iter()
    .find_map(|(pos, hit)| hit.then_some(pos))
}

///
/// clamp point to display bounds
///
/// # Arguments
///
/// * `prev_point`: coordinates, the cursor was before entering, within bounds of a display
/// * `entry_point`: point to clamp
///
/// returns: (i32, i32), the corrected entry point
///
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn clamp_to_display_bounds(
    display_regions: &[RECT],
    prev_point: (i32, i32),
    point: (i32, i32),
) -> (i32, i32) {
    /* find display where movement came from */
    let display = display_regions
        .iter()
        .find(|&d| is_within_dp_region(prev_point, d))
        .or_else(|| {
            // Hooks and monitor enumeration can disagree for a moment
            // (DPI change, display hot-plug, pointer in a gap of an
            // L-shaped layout). Never panic inside the hook: fall back to
            // the display containing the target point, and finally to the
            // whole desktop bounds.
            display_regions
                .iter()
                .find(|&d| is_within_dp_region(point, d))
        });

    let (x, y) = point;
    let (min_x, max_x, min_y, max_y) = match display {
        Some(display) => (
            display.left,
            display.right - 1,
            display.top,
            display.bottom - 1,
        ),
        None => {
            let Some((left, top, right, bottom)) = desktop_bounds(display_regions) else {
                return point;
            };
            (left, right - 1, top, bottom - 1)
        }
    };
    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn single_monitor_barrier_crossings_are_detected_on_all_four_edges() {
        let displays = [rect(0, 0, 1920, 1080)];

        assert_eq!(
            entered_barrier((0, 500), (-1, 500), &displays),
            Some(Position::Left)
        );
        assert_eq!(
            entered_barrier((1919, 500), (1920, 500), &displays),
            Some(Position::Right)
        );
        assert_eq!(
            entered_barrier((500, 0), (500, -1), &displays),
            Some(Position::Top)
        );
        assert_eq!(
            entered_barrier((500, 1079), (500, 1080), &displays),
            Some(Position::Bottom)
        );
        assert_eq!(entered_barrier((100, 100), (200, 200), &displays), None);
    }

    #[test]
    fn multi_monitor_shared_edge_does_not_trigger_barrier_between_displays() {
        // Two 1080p monitors side-by-side: [-1920..0) and [0..1920).
        let displays = [rect(-1920, 0, 0, 1080), rect(0, 0, 1920, 1080)];

        // Crossing from left monitor into right monitor must NOT trigger a barrier.
        assert_eq!(entered_barrier((-1, 500), (0, 500), &displays), None);
        assert_eq!(entered_barrier((0, 500), (-1, 500), &displays), None);

        // Crossing the outer edges triggers Left or Right.
        assert_eq!(
            entered_barrier((-1920, 500), (-1921, 500), &displays),
            Some(Position::Left)
        );
        assert_eq!(
            entered_barrier((1919, 500), (1920, 500), &displays),
            Some(Position::Right)
        );
    }

    #[test]
    fn clamped_cursor_pushed_against_each_edge_is_detected() {
        let displays = [rect(0, 0, 1920, 1080)];

        // The cursor is already clamped by Windows at the edge pixel; raw
        // input still points outward.
        assert_eq!(
            outward_edge_crossing((0, 500), (-3, 0), &displays),
            Some(Position::Left)
        );
        assert_eq!(
            outward_edge_crossing((1919, 500), (3, 0), &displays),
            Some(Position::Right)
        );
        assert_eq!(
            outward_edge_crossing((500, 0), (0, -3), &displays),
            Some(Position::Top)
        );
        assert_eq!(
            outward_edge_crossing((500, 1079), (0, 3), &displays),
            Some(Position::Bottom)
        );
    }

    #[test]
    fn movement_away_from_the_edge_is_not_a_crossing() {
        let displays = [rect(0, 0, 1920, 1080)];
        assert_eq!(outward_edge_crossing((0, 500), (3, 0), &displays), None);
        assert_eq!(outward_edge_crossing((1919, 500), (-3, 0), &displays), None);
        assert_eq!(outward_edge_crossing((960, 540), (3, 0), &displays), None);
        // Only one pixel inside: the OS would not have clamped the pointer.
        assert_eq!(outward_edge_crossing((1918, 500), (3, 0), &displays), None);
    }

    #[test]
    fn outward_crossing_uses_the_outer_desktop_edge_not_an_inner_monitor_edge() {
        // Two monitors: [-1920..0) and [0..1920).
        let displays = [rect(-1920, 0, 0, 1080), rect(0, 0, 1920, 1080)];

        // Cursor at the inner border (primary's left edge) must not trigger.
        assert_eq!(outward_edge_crossing((0, 500), (-2, 0), &displays), None);
        // Outer edges do.
        assert_eq!(
            outward_edge_crossing((-1920, 500), (-2, 0), &displays),
            Some(Position::Left)
        );
        assert_eq!(
            outward_edge_crossing((1919, 500), (2, 0), &displays),
            Some(Position::Right)
        );
    }

    #[test]
    fn outward_crossing_handles_negative_origin_and_stacked_monitors() {
        // Monitor above the primary, negative origin.
        let displays = [rect(0, 0, 1920, 1080), rect(0, -1080, 1920, 0)];
        assert_eq!(
            outward_edge_crossing((960, -1080), (0, -2), &displays),
            Some(Position::Top)
        );
        assert_eq!(
            outward_edge_crossing((960, 1079), (0, 2), &displays),
            Some(Position::Bottom)
        );
        // Primary's top edge is internal: no crossing.
        assert_eq!(outward_edge_crossing((960, 0), (0, -2), &displays), None);
    }

    #[test]
    fn outward_crossing_needs_at_least_one_display() {
        assert_eq!(outward_edge_crossing((0, 0), (-1, 0), &[]), None);
    }

    #[test]
    fn clamp_falls_back_instead_of_panicking_when_no_display_matches() {
        let displays = [rect(0, 0, 1920, 1080)];
        // `prev` is not inside any display, the target point is outside too.
        assert_eq!(
            clamp_to_display_bounds(&displays, (-50, -50), (5000, 5000)),
            (1919, 1079)
        );
        assert_eq!(
            clamp_to_display_bounds(&displays, (-50, -50), (-10, 500)),
            (0, 500)
        );
        assert_eq!(clamp_to_display_bounds(&[], (0, 0), (10, 10)), (10, 10));
    }

    #[test]
    fn clamp_pins_cursor_to_the_originating_display_bounds() {
        let displays = [rect(-1920, 0, 0, 1080), rect(0, 0, 1920, 1080)];

        // Left edge of the negative-origin display:
        assert_eq!(
            clamp_to_display_bounds(&displays, (-1910, 500), (-1950, 500)),
            (-1920, 500)
        );
        // Right edge of the primary display:
        assert_eq!(
            clamp_to_display_bounds(&displays, (1910, 500), (2000, 500)),
            (1919, 500)
        );
    }
}
