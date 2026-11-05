use super::error::{EmulationError, WindowsEmulationCreationError};
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    scancode,
};

use async_trait::async_trait;
use std::{
    ops::BitOrAssign,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use tokio::task::AbortHandle;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT_0, KEYEVENTF_EXTENDEDKEY, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{XBUTTON1, XBUTTON2};
use windows::Win32::{
    Foundation::{LPARAM, POINT, RECT, TRUE},
    Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO,
        MonitorFromPoint,
    },
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
            SetThreadDpiAwarenessContext,
        },
        Input::KeyboardAndMouse::{
            INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
            MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
            MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
            MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT,
        },
        WindowsAndMessaging::GetCursorPos,
    },
};
use windows::core::BOOL;

use super::{
    Emulation, EmulationHandle,
    absolute::{VirtualScreen, logical_to_physical},
};

const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_REPEAT_INTERVAL: Duration = Duration::from_millis(32);
/// How long the enumerated monitor layout is trusted. Display changes are
/// rare, and enumerating on every motion event would be wasteful.
const SCREEN_CACHE_TTL: Duration = Duration::from_secs(2);

pub(crate) struct WindowsEmulation {
    repeat_task: Option<AbortHandle>,
    /// Absolute moves bypass the Windows pointer acceleration curve, which
    /// would otherwise speed up every fast movement twice (once here, once in
    /// the sender). `LAN_MOUSE_WINDOWS_RELATIVE_MOUSE=1` restores the old
    /// relative path for troubleshooting.
    absolute: bool,
    /// Fractional part of the last motion, so slow movements are not lost to
    /// the integer-only `SendInput` delta.
    subpixel: super::subpixel::SubPixel,
    /// Virtual desktop bounds in the same space as `GetCursorPos`.
    screen: Option<VirtualScreen>,
    /// When `screen` was last enumerated.
    screen_checked: Option<Instant>,
    /// `(monitor handle, physical pixels per logical pixel, checked at)` for
    /// the cursor's current monitor.
    scale_cache: Option<(isize, f64, Instant)>,
}

impl WindowsEmulation {
    pub(crate) fn new() -> Result<Self, WindowsEmulationCreationError> {
        // Cursor coordinates and `EnumDisplayMonitors` only agree when the
        // thread sees physical pixels, which is also what makes the deltas
        // from the Windows sender match one-to-one.
        unsafe {
            let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        Ok(Self {
            repeat_task: None,
            absolute: !std::env::var("LAN_MOUSE_WINDOWS_RELATIVE_MOUSE").is_ok_and(|v| v == "1"),
            subpixel: Default::default(),
            screen: None,
            screen_checked: None,
            scale_cache: None,
        })
    }

    /// Apply a logical (96 DPI) motion using absolute coordinates.
    ///
    /// The wire carries logical pixels because the two machines can use
    /// different display scales: the sender divides its physical deltas by its
    /// own scale and the receiver multiplies by its own, so the pointer covers
    /// the same logical distance on both and the per-device sensitivity stays
    /// a pure multiplier.
    ///
    /// Relative `SendInput` movement is scaled by the system pointer speed and
    /// acceleration ("enhance pointer precision"), so a fast flick arrives
    /// amplified. Absolute moves map straight to a screen position and are not
    /// accelerated, which is what a KVM needs.
    fn move_absolute(&mut self, dx: f64, dy: f64) {
        let Some(current) = cursor_pos() else {
            let (move_x, move_y) = self.subpixel.consume(dx, dy);
            if move_x != 0 || move_y != 0 {
                rel_mouse(move_x, move_y);
            }
            return;
        };

        // Physical pixels per logical pixel on the monitor the cursor is on.
        let scale = self.scale_for(current);
        let (move_x, move_y) = self.subpixel.consume(
            logical_to_physical(dx, scale),
            logical_to_physical(dy, scale),
        );
        if move_x == 0 && move_y == 0 {
            return;
        }

        let cursor = (current.x, current.y);

        // Refresh the cached layout when it is stale, or when the cursor is
        // outside it (monitor hot-plug, DPI change, resolution change). A
        // failed enumeration is retried on the next TTL, not on every event.
        let stale = self
            .screen_checked
            .is_none_or(|checked| checked.elapsed() > SCREEN_CACHE_TTL);
        if stale || self.screen.is_some_and(|screen| !screen.contains(cursor)) {
            self.screen = unsafe { virtual_screen() };
            self.screen_checked = Some(Instant::now());
        }

        let Some(screen) = self.screen else {
            rel_mouse(move_x, move_y);
            return;
        };

        if !screen.contains(cursor) {
            // `GetCursorPos` and the monitor enumeration disagree. That should
            // not happen, but if it does, absolute coordinates would pin the
            // pointer to an edge; relative movement keeps it usable.
            warn_once(
                &SCREEN_MISMATCH_WARNED,
                "windows: cursor and monitor coordinates disagree, \
                 falling back to relative mouse movement",
            );
            rel_mouse(move_x, move_y);
            return;
        }

        // Clamp to the virtual desktop so the pointer cannot wrap, then
        // normalise to the 0..=65535 range that MOUSEEVENTF_VIRTUALDESK uses.
        let target = screen.clamp((
            cursor.0.saturating_add(move_x),
            cursor.1.saturating_add(move_y),
        ));
        let (normalised_x, normalised_y) = screen.normalise(target);

        send_mouse_input(MOUSEINPUT {
            dx: normalised_x,
            dy: normalised_y,
            mouseData: 0,
            dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            time: 0,
            dwExtraInfo: 0,
        });
    }

    /// Physical pixels per logical pixel on the monitor under `cursor`.
    ///
    /// Cached per monitor so a burst of motion events does not query the shell
    /// for every delta; the TTL picks up a scale change made while running.
    fn scale_for(&mut self, cursor: POINT) -> f64 {
        let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
        let id = monitor.0 as isize;
        if let Some((cached_id, scale, checked)) = self.scale_cache {
            if cached_id == id && checked.elapsed() < SCREEN_CACHE_TTL {
                return scale;
            }
        }
        let scale = unsafe { monitor_scale(monitor) };
        self.scale_cache = Some((id, scale, Instant::now()));
        scale
    }
}

fn cursor_pos() -> Option<POINT> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.is_ok().then_some(point)
}

/// Physical pixels per logical pixel for `monitor`.
///
/// `GetDpiForMonitor` only reports the real scale for a per-monitor DPI aware
/// process, which `main` sets up on Windows; otherwise it returns 96 and the
/// movement stays unscaled.
unsafe fn monitor_scale(monitor: HMONITOR) -> f64 {
    let mut dpi_x = 96u32;
    let mut dpi_y = 96u32;
    if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.is_ok()
        && dpi_x >= 96
    {
        f64::from(dpi_x) / 96.0
    } else {
        1.0
    }
}

#[async_trait]
impl Emulation for WindowsEmulation {
    async fn consume(&mut self, event: Event, _: EmulationHandle) -> Result<(), EmulationError> {
        match event {
            Event::Pointer(pointer_event) => match pointer_event {
                PointerEvent::Motion { time: _, dx, dy } => {
                    if self.absolute {
                        self.move_absolute(dx, dy);
                    } else {
                        // The escape hatch still speaks logical pixels on the
                        // wire, so convert those to physical deltas as well.
                        let scale = cursor_pos().map_or(1.0, |cursor| self.scale_for(cursor));
                        let (move_x, move_y) = self.subpixel.consume(
                            logical_to_physical(dx, scale),
                            logical_to_physical(dy, scale),
                        );
                        if move_x != 0 || move_y != 0 {
                            rel_mouse(move_x, move_y);
                        }
                    }
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => mouse_button(button, state),
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => scroll(axis, value as i32),
                PointerEvent::AxisDiscrete120 { axis, value } => scroll(axis, value),
            },
            Event::Keyboard(keyboard_event) => match keyboard_event {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    match state {
                        // pressed
                        0 => self.kill_repeat_task(),
                        1 => self.spawn_repeat_task(key).await,
                        _ => {}
                    }
                    key_event(key, state)
                }
                KeyboardEvent::Modifiers { .. } => {}
            },
        }
        // FIXME
        Ok(())
    }

    async fn create(&mut self, _handle: EmulationHandle) {}

    async fn destroy(&mut self, _handle: EmulationHandle) {}

    async fn terminate(&mut self) {}
}

impl WindowsEmulation {
    async fn spawn_repeat_task(&mut self, key: u32) {
        // there can only be one repeating key and it's
        // always the last to be pressed
        self.kill_repeat_task();
        let repeat_task = tokio::task::spawn_local(async move {
            tokio::time::sleep(DEFAULT_REPEAT_DELAY).await;
            loop {
                key_event(key, 1);
                tokio::time::sleep(DEFAULT_REPEAT_INTERVAL).await;
            }
        });
        self.repeat_task = Some(repeat_task.abort_handle());
    }
    fn kill_repeat_task(&mut self) {
        if let Some(task) = self.repeat_task.take() {
            task.abort();
        }
    }
}

fn send_input_safe(input: INPUT) {
    unsafe {
        loop {
            /* retval = number of successfully submitted events */
            if SendInput(&[input], std::mem::size_of::<INPUT>() as i32) > 0 {
                break;
            }
        }
    }
}

fn send_mouse_input(mi: MOUSEINPUT) {
    send_input_safe(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    });
}

fn send_keyboard_input(ki: KEYBDINPUT) {
    send_input_safe(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    });
}
fn rel_mouse(dx: i32, dy: i32) {
    let mi = MOUSEINPUT {
        dx,
        dy,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_MOVE,
        time: 0,
        dwExtraInfo: 0,
    };
    send_mouse_input(mi);
}

/// The virtual desktop bounds, in the same coordinate space as
/// `GetCursorPos`.
///
/// `GetSystemMetrics(SM_*VIRTUALSCREEN)` cannot be used here: it is documented
/// as not DPI aware and returns system-DPI (virtualised) values, while a
/// per-monitor-DPI-aware thread gets physical pixel coordinates from
/// `GetCursorPos`. On a scaled display that mismatch makes the bottom part of
/// the screen normalise to 65535, which sticks the pointer to the bottom edge.
unsafe fn virtual_screen() -> Option<VirtualScreen> {
    let mut monitors: Vec<(i32, i32, i32, i32)> = Vec::new();
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(monitor_enum_proc),
            LPARAM(&mut monitors as *mut Vec<(i32, i32, i32, i32)> as isize),
        )
    };
    if !ok.as_bool() {
        return None;
    }
    let screen = VirtualScreen::from_monitors(&monitors);
    if screen.is_none() {
        warn_once(
            &SCREEN_ENUM_WARNED,
            "windows: could not enumerate the display monitors, \
             falling back to relative mouse movement",
        );
    }
    screen
}

unsafe extern "system" fn monitor_enum_proc(
    monitor: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let monitors = unsafe { &mut *(data.0 as *mut Vec<(i32, i32, i32, i32)>) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        let rect = info.rcMonitor;
        monitors.push((
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
        ));
    }
    TRUE
}

/// `GetCursorPos` and the monitor enumeration must agree; warn only once per
/// run if they ever don't, so the hot path stays quiet.
static SCREEN_MISMATCH_WARNED: AtomicBool = AtomicBool::new(false);
/// A failed monitor enumeration is also only worth one log line.
static SCREEN_ENUM_WARNED: AtomicBool = AtomicBool::new(false);

fn warn_once(flag: &AtomicBool, message: &str) {
    if !flag.swap(true, Ordering::Relaxed) {
        log::warn!("{message}");
    }
}

fn mouse_button(button: u32, state: u32) {
    let dw_flags = match state {
        0 => match button {
            BTN_LEFT => MOUSEEVENTF_LEFTUP,
            BTN_RIGHT => MOUSEEVENTF_RIGHTUP,
            BTN_MIDDLE => MOUSEEVENTF_MIDDLEUP,
            BTN_BACK => MOUSEEVENTF_XUP,
            BTN_FORWARD => MOUSEEVENTF_XUP,
            _ => return,
        },
        1 => match button {
            BTN_LEFT => MOUSEEVENTF_LEFTDOWN,
            BTN_RIGHT => MOUSEEVENTF_RIGHTDOWN,
            BTN_MIDDLE => MOUSEEVENTF_MIDDLEDOWN,
            BTN_BACK => MOUSEEVENTF_XDOWN,
            BTN_FORWARD => MOUSEEVENTF_XDOWN,
            _ => return,
        },
        _ => return,
    };
    let mouse_data = match button {
        BTN_BACK => XBUTTON1 as u32,
        BTN_FORWARD => XBUTTON2 as u32,
        _ => 0,
    };
    let mi = MOUSEINPUT {
        dx: 0,
        dy: 0, // no movement
        mouseData: mouse_data,
        dwFlags: dw_flags,
        time: 0,
        dwExtraInfo: 0,
    };
    send_mouse_input(mi);
}

fn scroll(axis: u8, value: i32) {
    // WHEEL is positive up but HWHEEL positive right, like lan-mouse's horizontal axis
    let (event_type, value) = match axis {
        0 => (MOUSEEVENTF_WHEEL, value.saturating_neg()),
        1 => (MOUSEEVENTF_HWHEEL, value),
        _ => return,
    };
    let mi = MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: value as u32,
        dwFlags: event_type,
        time: 0,
        dwExtraInfo: 0,
    };
    send_mouse_input(mi);
}

fn key_event(key: u32, state: u8) {
    let scancode = match linux_keycode_to_windows_scancode(key) {
        Some(code) => code,
        None => return,
    };
    let extended = scancode > 0xff;
    let scancode = scancode & 0xff;
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags.bitor_assign(KEYEVENTF_EXTENDEDKEY);
    }
    if state == 0 {
        flags.bitor_assign(KEYEVENTF_KEYUP);
    }
    let ki = KEYBDINPUT {
        wVk: Default::default(),
        wScan: scancode,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    send_keyboard_input(ki);
}

fn linux_keycode_to_windows_scancode(linux_keycode: u32) -> Option<u16> {
    let linux_scancode = match scancode::Linux::try_from(linux_keycode) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("unknown keycode: {linux_keycode}");
            return None;
        }
    };
    log::trace!("linux code: {linux_scancode:?}");
    let windows_scancode = match scancode::Windows::try_from(linux_scancode) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("failed to translate linux code into windows scancode: {linux_scancode:?}");
            return None;
        }
    };
    log::trace!("windows code: {windows_scancode:?}");
    Some(windows_scancode as u16)
}
