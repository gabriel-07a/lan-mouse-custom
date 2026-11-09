use async_trait::async_trait;
use std::ptr;
use x11::{
    xlib::{self, XCloseDisplay},
    xtest,
};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
};

use crate::error::EmulationError;

use super::{Emulation, EmulationHandle, error::X11EmulationCreationError};

/// X11 core-protocol keycodes are evdev keycodes shifted by 8.
const X11_KEYCODE_OFFSET: u32 = 8;

/// Upper bound of X scroll clicks synthesized for a single discrete event. A
/// hostile/buggy peer could send an enormous value; without the cap that would
/// turn into millions of XTest requests and stall the event loop.
const MAX_SCROLL_CLICKS_PER_EVENT: u32 = 10;

/// Translate a Linux evdev keycode into an X11 core-protocol keycode.
///
/// Returns `None` when the keycode falls outside the range the X server
/// accepts. Passing it anyway makes the server return a BadValue error, and
/// Xlib's default error handler terminates the process — a remote peer pressing
/// e.g. KEY_MICMUTE (evdev 248 -> X keycode 256) used to kill the receiver.
fn x11_keycode(linux_key: u32, min_keycode: u32, max_keycode: u32) -> Option<u8> {
    let keycode = linux_key.checked_add(X11_KEYCODE_OFFSET)?;
    if keycode < min_keycode || keycode > max_keycode || keycode > u32::from(u8::MAX) {
        return None;
    }
    Some(keycode as u8)
}

/// Number of wheel clicks that a 120-unit discrete scroll event represents.
fn scroll_clicks(value: i32) -> u32 {
    if value == 0 {
        return 0;
    }
    (value.unsigned_abs() / 120).clamp(1, MAX_SCROLL_CLICKS_PER_EVENT)
}

pub(crate) struct X11Emulation {
    display: *mut xlib::Display,
    /// Fractional part of the last motion, so slow movements are not lost to
    /// the integer-only XTest delta.
    subpixel: super::subpixel::SubPixel,
    /// Keycode range accepted by the X server (see ::ref x11_keycode).
    min_keycode: u32,
    max_keycode: u32,
}

unsafe impl Send for X11Emulation {}

impl X11Emulation {
    pub(crate) fn new() -> Result<Self, X11EmulationCreationError> {
        let display = unsafe {
            match xlib::XOpenDisplay(ptr::null()) {
                d if std::ptr::eq(d, ptr::null_mut::<xlib::Display>()) => {
                    Err(X11EmulationCreationError::OpenDisplay)
                }
                display => Ok(display),
            }
        }?;

        // XTest is optional in the X server. Without this check the backend
        // would be selected and then silently do nothing (or, for extension
        // requests, hit Xlib's default error handler).
        let mut event_base = 0;
        let mut error_base = 0;
        let mut major = 0;
        let mut minor = 0;
        let available = unsafe {
            xtest::XTestQueryExtension(
                display,
                &mut event_base,
                &mut error_base,
                &mut major,
                &mut minor,
            )
        };
        if available == 0 {
            unsafe { XCloseDisplay(display) };
            return Err(X11EmulationCreationError::XTestNotAvailable);
        }

        let mut min_keycode = 0;
        let mut max_keycode = 0;
        let queried =
            unsafe { xlib::XDisplayKeycodes(display, &mut min_keycode, &mut max_keycode) };
        let (min_keycode, max_keycode) =
            if queried == 0 || min_keycode < 0 || max_keycode < min_keycode {
                log::warn!("could not query the X11 keycode range; assuming 8..=255");
                (8, 255)
            } else {
                (min_keycode as u32, max_keycode as u32)
            };

        Ok(Self {
            display,
            subpixel: Default::default(),
            min_keycode,
            max_keycode,
        })
    }

    fn relative_motion(&self, dx: i32, dy: i32) {
        unsafe {
            xtest::XTestFakeRelativeMotionEvent(self.display, dx, dy, 0, 0);
        }
    }

    fn emulate_mouse_button(&self, button: u32, state: u32) {
        let x11_button = match button {
            BTN_RIGHT => 3,
            BTN_MIDDLE => 2,
            BTN_BACK => 8,
            BTN_FORWARD => 9,
            BTN_LEFT => 1,
            // Unknown button: never fall back to the left button — pressing
            // left when the peer pressed a side button can start a drag.
            _ => return,
        };
        let pressed = i32::from(state != 0);
        unsafe {
            xtest::XTestFakeButtonEvent(self.display, x11_button, pressed, 0);
        }
    }

    const SCROLL_UP: u32 = 4;
    const SCROLL_DOWN: u32 = 5;
    const SCROLL_LEFT: u32 = 6;
    const SCROLL_RIGHT: u32 = 7;

    fn scroll_button(axis: u8, value: f64) -> Option<u32> {
        match axis {
            0 => Some(if value < 0.0 {
                Self::SCROLL_UP
            } else {
                Self::SCROLL_DOWN
            }),
            1 => Some(if value < 0.0 {
                Self::SCROLL_LEFT
            } else {
                Self::SCROLL_RIGHT
            }),
            _ => None,
        }
    }

    fn click_scroll_button(&self, direction: u32) {
        unsafe {
            xtest::XTestFakeButtonEvent(self.display, direction, 1, 0);
            xtest::XTestFakeButtonEvent(self.display, direction, 0, 0);
        }
    }

    fn emulate_scroll(&self, axis: u8, value: f64) {
        // A zero-value axis event (frame boundary) must not click a wheel
        // button: some touchpads send them.
        if !value.is_finite() || value == 0.0 {
            return;
        }
        if let Some(direction) = Self::scroll_button(axis, value) {
            self.click_scroll_button(direction);
        }
    }

    fn emulate_scroll_discrete(&self, axis: u8, value: i32) {
        // Sign doubles as the fractional value for the direction lookup.
        let Some(direction) = Self::scroll_button(axis, f64::from(value)) else {
            return;
        };
        for _ in 0..scroll_clicks(value) {
            self.click_scroll_button(direction);
        }
    }

    fn emulate_key(&self, key: u32, state: u8) {
        let Some(keycode) = x11_keycode(key, self.min_keycode, self.max_keycode) else {
            warn_unsupported_keycode(key);
            return;
        };
        let pressed = i32::from(state != 0);
        unsafe {
            xtest::XTestFakeKeyEvent(self.display, u32::from(keycode), pressed, 0);
        }
    }
}

/// Warn once about keycodes the X server cannot represent. Holding such a key
/// (e.g. mic mute) would otherwise log on every repeat event.
fn warn_unsupported_keycode(key: u32) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        log::warn!(
            "X11 cannot emulate Linux keycode {key} (out of the server's keycode \
             range); further warnings suppressed"
        );
    }
}

impl Drop for X11Emulation {
    fn drop(&mut self) {
        unsafe {
            XCloseDisplay(self.display);
        }
    }
}

#[async_trait]
impl Emulation for X11Emulation {
    async fn consume(&mut self, event: Event, _: EmulationHandle) -> Result<(), EmulationError> {
        match event {
            Event::Pointer(pointer_event) => match pointer_event {
                PointerEvent::Motion { time: _, dx, dy } => {
                    let (move_x, move_y) = self.subpixel.consume(dx, dy);
                    if move_x != 0 || move_y != 0 {
                        self.relative_motion(move_x, move_y);
                    }
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => {
                    self.emulate_mouse_button(button, state);
                }
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => {
                    self.emulate_scroll(axis, value);
                }
                PointerEvent::AxisDiscrete120 { axis, value } => {
                    self.emulate_scroll_discrete(axis, value);
                }
            },
            Event::Keyboard(KeyboardEvent::Key {
                time: _,
                key,
                state,
            }) => {
                self.emulate_key(key, state);
            }
            _ => {}
        }
        unsafe {
            xlib::XFlush(self.display);
        }
        // FIXME
        Ok(())
    }

    async fn create(&mut self, _: EmulationHandle) {
        // for our purposes it does not matter what client sent the event
    }

    async fn destroy(&mut self, _: EmulationHandle) {
        // for our purposes it does not matter what client sent the event
    }

    async fn terminate(&mut self) {
        /* nothing to do */
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keycodes_are_shifted_by_eight() {
        // evdev 30 = KEY_A, X11 keycode 38.
        assert_eq!(x11_keycode(30, 8, 255), Some(38));
        assert_eq!(x11_keycode(1, 8, 255), Some(9));
        assert_eq!(x11_keycode(8, 8, 255), Some(16));
    }

    /// Regression: evdev 248 (KEY_MICMUTE) maps to X11 keycode 256, which is
    /// above the server maximum. Sending it makes the X server raise BadValue
    /// and Xlib's default error handler kills the process.
    #[test]
    fn keycodes_outside_the_server_range_are_rejected() {
        assert_eq!(x11_keycode(248, 8, 255), None);
        assert_eq!(x11_keycode(0, 8, 255), Some(8));
        assert_eq!(x11_keycode(255, 8, 255), None, "256 is out of range");
        // A server with an unusual minimum must still be respected.
        assert_eq!(x11_keycode(1, 10, 255), None);
    }

    #[test]
    fn discrete_scroll_is_multi_tick_and_capped() {
        assert_eq!(scroll_clicks(0), 0);
        assert_eq!(scroll_clicks(120), 1);
        assert_eq!(scroll_clicks(60), 1);
        assert_eq!(scroll_clicks(240), 2);
        assert_eq!(scroll_clicks(-480), 4);
        assert_eq!(
            scroll_clicks(i32::MAX),
            MAX_SCROLL_CLICKS_PER_EVENT,
            "a hostile value must not trigger millions of XTest requests"
        );
    }

    #[test]
    fn only_vertical_and_horizontal_axes_scroll() {
        assert_eq!(
            X11Emulation::scroll_button(0, -1.0),
            Some(X11Emulation::SCROLL_UP)
        );
        assert_eq!(
            X11Emulation::scroll_button(0, 1.0),
            Some(X11Emulation::SCROLL_DOWN)
        );
        assert_eq!(
            X11Emulation::scroll_button(1, -1.0),
            Some(X11Emulation::SCROLL_LEFT)
        );
        assert_eq!(
            X11Emulation::scroll_button(1, 1.0),
            Some(X11Emulation::SCROLL_RIGHT)
        );
        assert_eq!(X11Emulation::scroll_button(2, 1.0), None);
        assert_eq!(X11Emulation::scroll_button(255, -1.0), None);
    }
}
