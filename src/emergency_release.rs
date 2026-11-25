//! The way home that no setting can take away: while this PC's keyboard and
//! mouse drive a peer, holding **Right Shift alone for 3 seconds** and letting
//! go brings them back here.
//!
//! The configurable release bind (Ctrl+Shift+Super+Alt by default) can be
//! changed or forgotten; this one is fixed so the owner is never locked out
//! of their own desktop when a peer stops responding. It only fires when
//! nothing else was pressed meanwhile, so Shift+letters and a long Shift
//! during a drag never trigger it.

use std::time::{Duration, Instant};

use input_event::{Event, KeyboardEvent, PointerEvent};

/// Linux key code of Right Shift (the capture backends report Linux codes).
const KEY_RIGHTSHIFT: u32 = 54;

/// How long Right Shift must be held.
pub(crate) const HOLD: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
pub(crate) struct EmergencyRelease {
    /// when Right Shift went down, while nothing else was pressed since
    held_since: Option<Instant>,
}

impl EmergencyRelease {
    /// Feed one captured event; `true` means: release the capture now.
    pub(crate) fn on_event(&mut self, event: &Event, now: Instant) -> bool {
        match event {
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) if *key == KEY_RIGHTSHIFT => {
                if *state != 0 {
                    // key repeat keeps the original press time
                    self.held_since.get_or_insert(now);
                    false
                } else {
                    self.held_since
                        .take()
                        .is_some_and(|since| now.saturating_duration_since(since) >= HOLD)
                }
            }
            // anything else pressed or clicked while Shift is down: it was
            // a chord, not the escape
            Event::Keyboard(KeyboardEvent::Key { state, .. }) if *state != 0 => {
                self.held_since = None;
                false
            }
            Event::Pointer(PointerEvent::Button { state, .. }) if *state != 0 => {
                self.held_since = None;
                false
            }
            _ => false,
        }
    }

    /// A hold that started at `since` (tests).
    #[cfg(test)]
    pub(crate) fn held_since_for_test(since: Instant) -> Self {
        Self {
            held_since: Some(since),
        }
    }

    /// Forget a half-done hold (the capture ended some other way).
    pub(crate) fn reset(&mut self) {
        self.held_since = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: u32, state: u8) -> Event {
        Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key,
            state,
        })
    }

    #[test]
    fn holding_right_shift_for_three_seconds_releases() {
        let t0 = Instant::now();
        let mut escape = EmergencyRelease::default();
        assert!(!escape.on_event(&key(KEY_RIGHTSHIFT, 1), t0));
        // auto-repeat while held
        assert!(!escape.on_event(&key(KEY_RIGHTSHIFT, 2), t0 + Duration::from_secs(1)));
        assert!(escape.on_event(&key(KEY_RIGHTSHIFT, 0), t0 + HOLD));
    }

    #[test]
    fn a_short_shift_does_not_release() {
        let t0 = Instant::now();
        let mut escape = EmergencyRelease::default();
        escape.on_event(&key(KEY_RIGHTSHIFT, 1), t0);
        assert!(!escape.on_event(&key(KEY_RIGHTSHIFT, 0), t0 + Duration::from_millis(2900)));
    }

    #[test]
    fn shift_with_another_key_or_click_is_a_chord() {
        let t0 = Instant::now();
        let mut escape = EmergencyRelease::default();
        escape.on_event(&key(KEY_RIGHTSHIFT, 1), t0);
        escape.on_event(&key(30, 1), t0 + Duration::from_millis(100)); // A
        assert!(!escape.on_event(&key(KEY_RIGHTSHIFT, 0), t0 + HOLD));

        escape.on_event(&key(KEY_RIGHTSHIFT, 1), t0);
        let click = Event::Pointer(PointerEvent::Button {
            time: 0,
            button: 0x110,
            state: 1,
        });
        escape.on_event(&click, t0 + Duration::from_secs(1));
        assert!(!escape.on_event(&key(KEY_RIGHTSHIFT, 0), t0 + HOLD));
    }

    #[test]
    fn motion_and_releases_do_not_cancel_the_hold() {
        let t0 = Instant::now();
        let mut escape = EmergencyRelease::default();
        escape.on_event(&key(KEY_RIGHTSHIFT, 1), t0);
        let motion = Event::Pointer(PointerEvent::Motion {
            time: 0,
            dx: 3.0,
            dy: 1.0,
        });
        escape.on_event(&motion, t0 + Duration::from_secs(1));
        escape.on_event(&key(30, 0), t0 + Duration::from_secs(2)); // a key-up
        assert!(escape.on_event(&key(KEY_RIGHTSHIFT, 0), t0 + HOLD));
    }

    #[test]
    fn left_shift_is_not_the_escape() {
        let t0 = Instant::now();
        let mut escape = EmergencyRelease::default();
        escape.on_event(&key(42, 1), t0);
        assert!(!escape.on_event(&key(42, 0), t0 + HOLD));
    }
}
