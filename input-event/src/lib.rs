use std::fmt::{self, Display};

pub mod error;
pub mod scancode;

/// Tracks input that lan-mouse itself injected into the local OS.
///
/// Capture backends run in the same process as emulation. On Windows the
/// injected `SendInput` motion is visible to the low level hook / raw input and
/// would otherwise look like the local user pushing against a screen edge,
/// making two machines hand control back and forth at the shared border.
/// Emulation marks every injected event here; capture ignores barrier
/// crossings that happen right after an injection.
pub mod injection {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Timestamp (milliseconds) of the last injected input, 0 = never.
    static LAST_INJECTED_MS: AtomicU64 = AtomicU64::new(0);

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0)
    }

    /// Record that input was just injected into the OS.
    pub fn mark() {
        LAST_INJECTED_MS.store(now_ms(), Ordering::Relaxed);
    }

    /// Whether input was injected within the last `window_ms` milliseconds.
    pub fn is_recent(window_ms: u64) -> bool {
        let last = LAST_INJECTED_MS.load(Ordering::Relaxed);
        last != 0 && now_ms().saturating_sub(last) < window_ms
    }
}

#[cfg(all(unix, feature = "libei", not(target_os = "macos")))]
mod libei;

// FIXME
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_BACK: u32 = 0x113;
pub const BTN_FORWARD: u32 = 0x114;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum PointerEvent {
    /// relative motion event
    Motion { time: u32, dx: f64, dy: f64 },
    /// mouse button event
    Button { time: u32, button: u32, state: u32 },
    /// axis event, scroll event for touchpads
    Axis { time: u32, axis: u8, value: f64 },
    /// discrete axis event, scroll event for mice - 120 = one scroll tick
    AxisDiscrete120 { axis: u8, value: i32 },
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum KeyboardEvent {
    /// a key press / release event
    Key { time: u32, key: u32, state: u8 },
    /// modifiers changed state
    Modifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
}

#[derive(PartialEq, Debug, Clone, Copy)]
pub enum Event {
    /// pointer event (motion / button / axis)
    Pointer(PointerEvent),
    /// keyboard events (key / modifiers)
    Keyboard(KeyboardEvent),
}

impl Display for PointerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PointerEvent::Motion { time: _, dx, dy } => write!(f, "motion({dx},{dy})"),
            PointerEvent::Button {
                time: _,
                button,
                state,
            } => {
                let str = match *button {
                    BTN_LEFT => Some("left"),
                    BTN_RIGHT => Some("right"),
                    BTN_MIDDLE => Some("middle"),
                    BTN_FORWARD => Some("forward"),
                    BTN_BACK => Some("back"),
                    _ => None,
                };
                if let Some(button) = str {
                    write!(f, "button({button}, {state})")
                } else {
                    write!(f, "button({button}, {state}")
                }
            }
            PointerEvent::Axis {
                time: _,
                axis,
                value,
            } => write!(f, "scroll({axis}, {value})"),
            PointerEvent::AxisDiscrete120 { axis, value } => {
                write!(f, "scroll-120 ({axis}, {value})")
            }
        }
    }
}

impl Display for KeyboardEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyboardEvent::Key {
                time: _,
                key,
                state,
            } => {
                let scan = scancode::Linux::try_from(*key);
                if let Ok(scan) = scan {
                    write!(f, "key({scan:?}, {state})")
                } else {
                    write!(f, "key({key}, {state})")
                }
            }
            KeyboardEvent::Modifiers {
                depressed: mods_depressed,
                latched: mods_latched,
                locked: mods_locked,
                group,
            } => write!(
                f,
                "modifiers({mods_depressed},{mods_latched},{mods_locked},{group})"
            ),
        }
    }
}

impl Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Pointer(p) => write!(f, "{p}"),
            Event::Keyboard(k) => write!(f, "{k}"),
        }
    }
}

#[cfg(test)]
mod injection_tests {
    use super::injection;

    #[test]
    fn injection_marker_is_recent_after_marking() {
        injection::mark();
        assert!(injection::is_recent(60_000));
        assert!(!injection::is_recent(0));
    }
}
