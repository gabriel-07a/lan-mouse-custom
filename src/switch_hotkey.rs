//! Hotkeys that move the keyboard and mouse between PCs while this PC's
//! keyboard drives one of them (the capture sees every key then):
//!
//! - **AltGr + Right Shift**, pressed together and let go with nothing else
//!   in between: the next PC (after the last one, back to this PC);
//! - **AltGr + Right Shift + 1 … 9**: straight to that PC (in the order of
//!   the device list); **+ 0**: back to this PC.
//!
//! AltGr is Right Alt on keyboards without it. The modifiers still reach the
//! peer (they are only modifiers), but the digit of a chord does not, so
//! AltGr+Shift+1 never types "¹" there. On this PC itself (nothing captured)
//! the same moves are `lan-mouse cli go <PC>` / `lan-mouse cli home`, which a
//! desktop shortcut can run.

use input_event::{Event, KeyboardEvent, PointerEvent};

/// Linux key codes (the capture backends report Linux codes).
const KEY_RIGHTALT: u32 = 100;
const KEY_RIGHTSHIFT: u32 = 54;
const KEY_1: u32 = 2;
const KEY_0: u32 = 11;

/// Where a hotkey sends the keyboard and mouse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SwitchTarget {
    /// the PC after the one being driven (or this PC after the last)
    Next,
    /// the n-th PC of the device list, 1-based
    Pc(u8),
    /// this PC
    Home,
}

/// What to do with one captured event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// forward it to the peer as usual
    Forward,
    /// part of a hotkey: do not forward
    Swallow,
    /// do not forward, and switch
    Switch(SwitchTarget),
}

#[derive(Debug, Default)]
pub(crate) struct SwitchHotkey {
    altgr: bool,
    shift: bool,
    /// both modifiers were down together and nothing else was pressed since
    chord: bool,
    /// a digit completed the chord; the release must not also mean "next"
    used: bool,
    /// digit keys held as part of a chord (their release is swallowed too)
    held_digit: Option<u32>,
    /// another key or a click while AltGr or Right Shift was down: no chord
    /// until both are up again (AltGr+Q, then Shift, is typing)
    tainted: bool,
}

impl SwitchHotkey {
    pub(crate) fn on_event(&mut self, event: &Event) -> Verdict {
        match event {
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => self.on_key(*key, *state),
            Event::Pointer(PointerEvent::Button { state, .. }) if *state != 0 => {
                // a click during the chord makes it something else
                self.chord = false;
                if self.altgr || self.shift {
                    self.tainted = true;
                }
                Verdict::Forward
            }
            _ => Verdict::Forward,
        }
    }

    fn on_key(&mut self, key: u32, state: u8) -> Verdict {
        let pressed = state != 0;
        match key {
            KEY_RIGHTALT | KEY_RIGHTSHIFT => {
                let was_both = self.altgr && self.shift;
                if key == KEY_RIGHTALT {
                    self.altgr = pressed;
                } else {
                    self.shift = pressed;
                }
                if pressed {
                    if self.altgr && self.shift && !was_both && state == 1 && !self.tainted {
                        self.chord = true;
                        self.used = false;
                    }
                    return Verdict::Forward;
                }
                if !self.altgr && !self.shift {
                    self.tainted = false;
                }
                // The first of the two keys let go ends the chord.
                if was_both {
                    let fire = self.chord && !self.used;
                    self.chord = false;
                    if fire {
                        return Verdict::Switch(SwitchTarget::Next);
                    }
                }
                Verdict::Forward
            }
            _ if self.held_digit == Some(key) => {
                if !pressed {
                    self.held_digit = None;
                }
                Verdict::Swallow
            }
            _ if pressed && self.altgr && self.shift && self.chord && digit(key).is_some() => {
                self.used = true;
                self.held_digit = Some(key);
                if state != 1 {
                    // auto-repeat of a digit already used
                    return Verdict::Swallow;
                }
                match digit(key) {
                    Some(0) => Verdict::Switch(SwitchTarget::Home),
                    Some(n) => Verdict::Switch(SwitchTarget::Pc(n)),
                    None => Verdict::Forward,
                }
            }
            _ => {
                if pressed {
                    // any other key: a normal AltGr/Shift combination
                    self.chord = false;
                    if self.altgr || self.shift {
                        self.tainted = true;
                    }
                }
                Verdict::Forward
            }
        }
    }

    /// Forget everything (the capture ended).
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

/// 1..9 for the top-row digit keys 1..9, 0 for the 0 key.
fn digit(key: u32) -> Option<u8> {
    match key {
        KEY_1..=10 => Some((key - KEY_1 + 1) as u8),
        KEY_0 => Some(0),
        _ => None,
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

    fn run(hotkey: &mut SwitchHotkey, events: &[(u32, u8)]) -> Vec<Verdict> {
        events
            .iter()
            .map(|&(k, s)| hotkey.on_event(&key(k, s)))
            .collect()
    }

    #[test]
    fn altgr_and_right_shift_alone_mean_next() {
        let mut hotkey = SwitchHotkey::default();
        let verdicts = run(
            &mut hotkey,
            &[
                (KEY_RIGHTALT, 1),
                (KEY_RIGHTSHIFT, 1),
                (KEY_RIGHTSHIFT, 0),
                (KEY_RIGHTALT, 0),
            ],
        );
        assert_eq!(
            verdicts,
            vec![
                Verdict::Forward,
                Verdict::Forward,
                Verdict::Switch(SwitchTarget::Next),
                Verdict::Forward
            ]
        );
    }

    #[test]
    fn either_order_works() {
        let mut hotkey = SwitchHotkey::default();
        let verdicts = run(
            &mut hotkey,
            &[
                (KEY_RIGHTSHIFT, 1),
                (KEY_RIGHTALT, 1),
                (KEY_RIGHTALT, 0),
                (KEY_RIGHTSHIFT, 0),
            ],
        );
        assert_eq!(verdicts[2], Verdict::Switch(SwitchTarget::Next));
    }

    #[test]
    fn a_digit_picks_a_pc_and_is_not_forwarded() {
        let mut hotkey = SwitchHotkey::default();
        let verdicts = run(
            &mut hotkey,
            &[
                (KEY_RIGHTALT, 1),
                (KEY_RIGHTSHIFT, 1),
                (KEY_1 + 1, 1), // the "2" key
                (KEY_1 + 1, 0),
                (KEY_RIGHTSHIFT, 0),
                (KEY_RIGHTALT, 0),
            ],
        );
        assert_eq!(
            verdicts,
            vec![
                Verdict::Forward,
                Verdict::Forward,
                Verdict::Switch(SwitchTarget::Pc(2)),
                Verdict::Swallow,
                Verdict::Forward, // no "next" on top of the digit
                Verdict::Forward
            ]
        );
    }

    #[test]
    fn zero_means_home() {
        let mut hotkey = SwitchHotkey::default();
        let verdicts = run(
            &mut hotkey,
            &[(KEY_RIGHTALT, 1), (KEY_RIGHTSHIFT, 1), (KEY_0, 1)],
        );
        assert_eq!(verdicts[2], Verdict::Switch(SwitchTarget::Home));
    }

    #[test]
    fn altgr_with_a_letter_types_normally() {
        let mut hotkey = SwitchHotkey::default();
        // AltGr + Q, then Shift: no hotkey, everything forwarded
        let verdicts = run(
            &mut hotkey,
            &[
                (KEY_RIGHTALT, 1),
                (16, 1),
                (16, 0),
                (KEY_RIGHTSHIFT, 1),
                (KEY_RIGHTSHIFT, 0),
                (KEY_RIGHTALT, 0),
            ],
        );
        assert!(
            verdicts.iter().all(|v| *v == Verdict::Forward),
            "{verdicts:?}"
        );

        // the chord, then a letter: not a hotkey either
        let verdicts = run(
            &mut hotkey,
            &[
                (KEY_RIGHTALT, 1),
                (KEY_RIGHTSHIFT, 1),
                (30, 1),
                (30, 0),
                (KEY_RIGHTSHIFT, 0),
                (KEY_RIGHTALT, 0),
            ],
        );
        assert!(
            verdicts.iter().all(|v| *v == Verdict::Forward),
            "{verdicts:?}"
        );
    }

    #[test]
    fn a_digit_without_the_chord_is_typed() {
        let mut hotkey = SwitchHotkey::default();
        assert_eq!(
            run(&mut hotkey, &[(KEY_1, 1), (KEY_1, 0)]),
            vec![Verdict::Forward; 2]
        );
        // Shift+1 alone types "!"
        let verdicts = run(
            &mut hotkey,
            &[
                (KEY_RIGHTSHIFT, 1),
                (KEY_1, 1),
                (KEY_1, 0),
                (KEY_RIGHTSHIFT, 0),
            ],
        );
        assert!(
            verdicts.iter().all(|v| *v == Verdict::Forward),
            "{verdicts:?}"
        );
    }

    #[test]
    fn key_repeat_of_the_modifiers_does_not_restart_the_chord() {
        let mut hotkey = SwitchHotkey::default();
        let verdicts = run(
            &mut hotkey,
            &[
                (KEY_RIGHTALT, 1),
                (KEY_RIGHTSHIFT, 1),
                (KEY_0, 1),
                (KEY_0, 0),
                (KEY_RIGHTSHIFT, 2), // auto-repeat
                (KEY_RIGHTSHIFT, 0),
            ],
        );
        assert_eq!(verdicts[2], Verdict::Switch(SwitchTarget::Home));
        assert_eq!(
            verdicts[5],
            Verdict::Forward,
            "the digit already used the chord"
        );
    }

    #[test]
    fn a_click_cancels_the_chord() {
        let mut hotkey = SwitchHotkey::default();
        run(&mut hotkey, &[(KEY_RIGHTALT, 1), (KEY_RIGHTSHIFT, 1)]);
        let click = Event::Pointer(PointerEvent::Button {
            time: 0,
            button: 0x110,
            state: 1,
        });
        hotkey.on_event(&click);
        assert_eq!(
            run(&mut hotkey, &[(KEY_RIGHTSHIFT, 0)]),
            vec![Verdict::Forward]
        );
    }
}
