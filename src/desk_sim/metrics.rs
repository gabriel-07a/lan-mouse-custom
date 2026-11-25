//! Fluidity metrics of a desk, read from what the virtual screens logged:
//! how long a crossing took to move the cursor on the other screen, whether
//! it came out where it went in, and whether anything was left pressed.
//! The fluidity matrix (`fluidity.rs`) holds every OS pair to these.

use std::time::Duration;

use input_capture::Position;

use super::{Desk, ScreenEvent};

/// The longest a crossing may take to move the cursor on the other screen:
/// one frame at 60 Hz.
pub(crate) const MAX_CROSSING_LATENCY: Duration = Duration::from_millis(16);
/// How far, in px of the receiving screen, the entry point may be from the
/// proportional position of the crossing point.
pub(crate) const MAX_ENTRY_ERROR_PX: f64 = 1.5;

impl Desk {
    /// Time from `source`'s last capture begin to the first motion injected
    /// on `target` after it (`None`: no such pair yet).
    pub(crate) fn crossing_latency(&self, source: &str, target: &str) -> Option<Duration> {
        let begin = self
            .pc(source)
            .state()
            .log
            .iter()
            .rev()
            .find(|l| matches!(l.event, ScreenEvent::CaptureBegin { .. }))?
            .at;
        let first = self
            .pc(target)
            .state()
            .log
            .iter()
            .find(|l| l.at >= begin && matches!(l.event, ScreenEvent::FirstInjectedMotion))?
            .at;
        Some(first.duration_since(begin))
    }

    /// Distance, in px along the receiving edge, between where `target`'s
    /// cursor entered and the proportional position of where `source`'s
    /// cursor crossed (last crossing).
    pub(crate) fn entry_error_px(&self, source: &str, target: &str) -> Option<f64> {
        let (crossed_at, along) = self
            .pc(source)
            .state()
            .log
            .iter()
            .rev()
            .find_map(|l| match l.event {
                ScreenEvent::CaptureBegin { along, .. } => Some((l.at, along)),
                _ => None,
            })?;
        let state = self.pc(target).state();
        let entered = state.log.iter().find_map(|l| match l.event {
            ScreenEvent::RemoteBegin { edge, along } if l.at >= crossed_at => Some((edge, along)),
            _ => None,
        })?;
        let (edge, Some(entry)) = entered else {
            return None;
        };
        let span = match edge {
            input_emulation::PointerEdge::Left | input_emulation::PointerEdge::Right => {
                state.height - 1.0
            }
            _ => state.width - 1.0,
        };
        Some((f64::from(entry) - f64::from(along)).abs() * span)
    }

    /// Buttons and keys an app on some PC still believes are down although
    /// nobody holds them: the stuck-modifier / stuck-drag class of bugs.
    pub(crate) fn stuck_inputs(&self) -> Vec<String> {
        // what somebody still physically holds may legitimately be down; read
        // it before locking any one screen (the screens' locks are not
        // reentrant)
        let physically_held: std::collections::HashSet<u32> = self
            .pcs
            .iter()
            .flat_map(|pc| pc.state().physical_buttons.clone())
            .collect();
        let mut stuck = Vec::new();
        for pc in &self.pcs {
            let state = pc.state();
            let held: Vec<u32> = state
                .app_buttons
                .iter()
                .copied()
                .filter(|button| !physically_held.contains(button))
                .collect();
            if !held.is_empty() {
                stuck.push(format!("{}: buttons {held:?} stuck", pc.name));
            }
            if !state.app_keys.is_empty() {
                stuck.push(format!("{}: keys {:?} stuck", pc.name, state.app_keys));
            }
        }
        stuck
    }

    /// `source`'s cursor is parked on its `side` edge at `along`, ready to
    /// cross: walks there from wherever it is.
    pub(crate) async fn approach_edge(&self, source: &str, side: Position, along: f64) {
        let (width, height) = {
            let state = self.pc(source).state();
            (state.width, state.height)
        };
        let (x, y) = self.cursor(source);
        // a point 6 px inside the edge, at `along`
        let (tx, ty) = match side {
            Position::Left => (6.0, along * (height - 1.0)),
            Position::Right => (width - 7.0, along * (height - 1.0)),
            Position::Top => (along * (width - 1.0), 6.0),
            Position::Bottom => (along * (width - 1.0), height - 7.0),
        };
        // along the edge first, then towards it: never brush another edge
        match side {
            Position::Left | Position::Right => {
                self.move_mouse_on(source, 0.0, ty - y).await;
                self.move_mouse_on(source, tx - x, 0.0).await;
            }
            Position::Top | Position::Bottom => {
                self.move_mouse_on(source, tx - x, 0.0).await;
                self.move_mouse_on(source, 0.0, ty - y).await;
            }
        }
    }
}

/// Unit vector out of a screen through `side`.
pub(crate) fn outward(side: Position) -> (f64, f64) {
    match side {
        Position::Left => (-1.0, 0.0),
        Position::Right => (1.0, 0.0),
        Position::Top => (0.0, -1.0),
        Position::Bottom => (0.0, 1.0),
    }
}
