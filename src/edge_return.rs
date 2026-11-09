//! Software return-hand-off detection for machines that receive input.
//!
//! Only the Windows emulation backend can report a pointer being pushed past a
//! screen edge. On Wayland/libei/X11/portal backends the injected pointer does
//! not reliably trigger capture barriers, so a controlled machine had no way to
//! ask the controller for its cursor back: the cursor stayed trapped on the
//! controlled screen.
//!
//! The controller sends 1:1 pixel motion deltas. When the pointer enters this
//! machine through an edge, the deltas since that moment sum up to the cursor
//! distance from the entry edge inside the screen. Pushing the pointer back out
//! (the local cursor clamps at the border while the controller keeps sending
//! outward deltas) drives that sum negative by a few pixels. That overshoot is
//! a reliable, low-noise signal for a deliberate return hand-off, without
//! needing absolute pointer positions or compositor support.

use std::collections::HashMap;
use std::net::SocketAddr;

use input_event::{Event, PointerEvent};
use lan_mouse_ipc::Position;

/// How far (in pixels) the pointer has to be pushed outward, in one unbroken
/// push, before the hand-off fires.
///
/// The protocol carries relative motion only and never transfers the receiver's
/// absolute pointer position, so this tracker cannot know how far the cursor is
/// from the screen edge. Requiring a sustained outward push (a deliberate
/// gesture; ordinary navigation back-and-forth reverses long before this)
/// distinguishes "the user is pushing the pointer out through the edge" from a
/// normal cursor movement.
const RETURN_PUSH_PX: f64 = 250.0;

/// The pointer must first travel this far into the screen before a return can
/// be detected. This swallows jitter right at the moment of entering, when the
/// cursor is still clamped on the edge.
const ARM_DISTANCE_PX: f64 = 24.0;

#[derive(Default)]
pub(crate) struct EdgeReturnTracker {
    sessions: HashMap<SocketAddr, Session>,
}

struct Session {
    /// Edge through which the peer entered this screen.
    edge: Position,
    /// Signed distance from the entry edge into the screen (positive = inward).
    inward: f64,
    /// Length of the current unbroken outward push (resets on inward motion).
    outward_push: f64,
    /// Set once the pointer moved far enough inward to make a return deliberate.
    armed: bool,
}

impl EdgeReturnTracker {
    /// Register a new incoming session that entered through `edge`.
    pub(crate) fn enter(&mut self, addr: SocketAddr, edge: Position) {
        self.sessions.insert(
            addr,
            Session {
                edge,
                inward: 0.0,
                outward_push: 0.0,
                armed: false,
            },
        );
    }

    /// Drop the state of a session that ended.
    pub(crate) fn leave(&mut self, addr: SocketAddr) {
        self.sessions.remove(&addr);
    }

    /// Feed one injected input event.
    ///
    /// Returns the entry edge when the user deliberately pushed the pointer back
    /// out through it. The session state is consumed in that case: the caller
    /// sends the hand-off request and the next `Enter` starts a fresh session.
    pub(crate) fn observe(&mut self, addr: SocketAddr, event: &Event) -> Option<Position> {
        let Event::Pointer(PointerEvent::Motion { dx, dy, .. }) = event else {
            return None;
        };
        if !dx.is_finite() || !dy.is_finite() {
            return None;
        }
        let session = self.sessions.get_mut(&addr)?;
        let inward_delta = match session.edge {
            Position::Left => *dx,
            Position::Right => -*dx,
            Position::Top => *dy,
            Position::Bottom => -*dy,
        };
        session.inward += inward_delta;
        if session.inward >= ARM_DISTANCE_PX {
            session.armed = true;
        }
        if inward_delta < 0.0 {
            session.outward_push += -inward_delta;
        } else {
            // Any inward motion interrupts the push: an ordinary reversal must
            // not be mistaken for pushing the pointer out through the edge.
            session.outward_push = (session.outward_push - inward_delta).max(0.0);
        }
        if session.armed && session.outward_push >= RETURN_PUSH_PX {
            let edge = session.edge;
            self.sessions.remove(&addr);
            return Some(edge);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn addr(last: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, last)), 4242)
    }

    fn motion(dx: f64, dy: f64) -> Event {
        Event::Pointer(PointerEvent::Motion { time: 0, dx, dy })
    }

    #[test]
    fn moving_inward_never_returns() {
        let mut tracker = EdgeReturnTracker::default();
        tracker.enter(addr(1), Position::Left);
        for _ in 0..50 {
            assert_eq!(tracker.observe(addr(1), &motion(10.0, 0.0)), None);
        }
    }

    #[test]
    fn pushing_back_out_through_the_entry_edge_returns_once() {
        let mut tracker = EdgeReturnTracker::default();
        tracker.enter(addr(1), Position::Left);
        assert_eq!(tracker.observe(addr(1), &motion(80.0, 0.0)), None);
        // Move around inside the screen.
        assert_eq!(tracker.observe(addr(1), &motion(40.0, 25.0)), None);
        assert_eq!(tracker.observe(addr(1), &motion(0.0, -25.0)), None);
        // A deliberate, unbroken push back out through the edge.
        let mut returned = None;
        for _ in 0..40 {
            if let Some(edge) = tracker.observe(addr(1), &motion(-10.0, 0.0)) {
                returned = Some(edge);
                break;
            }
        }
        assert_eq!(returned, Some(Position::Left));
        // The session is consumed: further motion does not fire again.
        assert_eq!(tracker.observe(addr(1), &motion(-10.0, 0.0)), None);
    }

    #[test]
    fn an_ordinary_reversal_does_not_return() {
        let mut tracker = EdgeReturnTracker::default();
        tracker.enter(addr(6), Position::Left);
        // Arm the tracker, then move around normally: reversals interrupt the
        // outward push long before the deliberate threshold.
        assert_eq!(tracker.observe(addr(6), &motion(200.0, 0.0)), None);
        for _ in 0..20 {
            assert_eq!(tracker.observe(addr(6), &motion(-10.0, 0.0)), None);
            assert_eq!(tracker.observe(addr(6), &motion(10.0, 3.0)), None);
        }
    }

    #[test]
    fn jitter_at_the_edge_right_after_entering_is_ignored() {
        let mut tracker = EdgeReturnTracker::default();
        tracker.enter(addr(1), Position::Left);
        // Small back-and-forth right after the entry, never arming.
        for _ in 0..10 {
            assert_eq!(tracker.observe(addr(1), &motion(-2.0, 0.0)), None);
            assert_eq!(tracker.observe(addr(1), &motion(2.0, 0.0)), None);
        }
    }

    #[test]
    fn every_edge_uses_its_own_axis_and_sign() {
        let cases = [
            (Position::Left, (1.0, 0.0), (-1.0, 0.0)),
            (Position::Right, (-1.0, 0.0), (1.0, 0.0)),
            (Position::Top, (0.0, 1.0), (0.0, -1.0)),
            (Position::Bottom, (0.0, -1.0), (0.0, 1.0)),
        ];
        for (edge, inward, outward) in cases {
            let mut tracker = EdgeReturnTracker::default();
            tracker.enter(addr(2), edge);
            assert_eq!(
                tracker.observe(addr(2), &motion(inward.0 * 100.0, inward.1 * 100.0)),
                None,
                "{edge:?} inward"
            );
            let mut returned = None;
            for _ in 0..40 {
                if let Some(found) =
                    tracker.observe(addr(2), &motion(outward.0 * 10.0, outward.1 * 10.0))
                {
                    returned = Some(found);
                    break;
                }
            }
            assert_eq!(returned, Some(edge), "{edge:?} outward");
        }
    }

    #[test]
    fn unknown_sessions_and_non_motion_events_are_ignored() {
        let mut tracker = EdgeReturnTracker::default();
        assert_eq!(tracker.observe(addr(3), &motion(-100.0, 0.0)), None);
        let mut tracker = EdgeReturnTracker::default();
        tracker.enter(addr(3), Position::Left);
        assert_eq!(tracker.observe(addr(4), &motion(-100.0, 0.0)), None);
        assert_eq!(
            tracker.observe(
                addr(3),
                &Event::Pointer(PointerEvent::Button {
                    time: 0,
                    button: 0x110,
                    state: 1,
                })
            ),
            None
        );
        tracker.leave(addr(3));
        assert_eq!(tracker.observe(addr(3), &motion(-100.0, 0.0)), None);
    }

    #[test]
    fn non_finite_deltas_are_ignored() {
        let mut tracker = EdgeReturnTracker::default();
        tracker.enter(addr(5), Position::Left);
        assert_eq!(tracker.observe(addr(5), &motion(f64::NAN, 0.0)), None);
        assert_eq!(tracker.observe(addr(5), &motion(0.0, f64::INFINITY)), None);
        assert_eq!(tracker.observe(addr(5), &motion(500.0, 0.0)), None);
    }
}
