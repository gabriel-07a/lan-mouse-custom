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
        .unwrap();

    /* clamp to bounds (inclusive) */
    let (x, y) = point;
    let (min_x, max_x) = (display.left, display.right - 1);
    let (min_y, max_y) = (display.top, display.bottom - 1);
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
