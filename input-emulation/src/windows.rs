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
        WindowsAndMessaging::{GetCursorPos, SetCursorPos},
    },
};
use windows::core::BOOL;

use super::{
    EdgeReturnNotifier, Emulation, EmulationHandle, PointerEdge,
    absolute::{
        ENTRY_ANCHOR_MARGIN, EdgeReturnLatch, VirtualScreen, entry_anchor,
        entry_anchor_fraction_in_monitors, logical_to_physical, overshoot_edge,
    },
};

const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_REPEAT_INTERVAL: Duration = Duration::from_millis(32);
// TODO(windows): read the user's SPI_GETKEYBOARDDELAY / SPI_GETKEYBOARDSPEED
// values instead of the OS defaults above so key repeat matches other
// applications on the receiver. Deferred: needs new `windows` crate features
// and real hardware validation.
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
    /// Service notifier: the remote pointer was pushed past a screen edge and
    /// the driving peer must release its capture (return hand-off).
    edge_return: Option<EdgeReturnNotifier>,
    /// Throttled, bounded retry state for hand-off requests. Re-armed when the
    /// remote pointer moves back inside the screen or the session ends.
    edge_return_latch: EdgeReturnLatch,
    /// Screen edge the peer entered through. Only pushing the remote pointer
    /// back out through *that* edge is the return hand-off; the other three
    /// edges must keep the cursor clamped on this screen.
    remote_edge: Option<PointerEdge>,
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
            edge_return: None,
            edge_return_latch: EdgeReturnLatch::default(),
            remote_edge: None,
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
            self.screen = unsafe { virtual_screen() }.map(|(screen, _)| screen);
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

        // The peer keeps pushing past one of our screen edges: that is the
        // return hand-off. Windows only clamps the pointer, it does not tell
        // the capturing side, so report it here and let the service ask the
        // peer to release its capture.
        let requested = (
            cursor.0.saturating_add(move_x),
            cursor.1.saturating_add(move_y),
        );
        match overshoot_edge(&screen, requested) {
            // Only the edge the peer entered through is a return hand-off.
            // Reporting every edge made the other PC take control back when
            // the cursor merely reached the top/bottom/opposite side.
            Some(edge) if self.remote_edge.is_none_or(|entry| entry == edge) => {
                let along = screen.edge_along(edge, requested);
                self.request_edge_return(edge, Some(along))
            }
            Some(_) => self.edge_return_latch.clear(),
            // Back inside the screen: the next edge contact is a new hand-off.
            None => self.edge_return_latch.clear(),
        }

        // Clamp to the virtual desktop so the pointer cannot wrap, then
        // normalise to the 0..=65535 range that MOUSEEVENTF_VIRTUALDESK uses.
        let target = screen.clamp(requested);
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
                        // released
                        0 => self.kill_repeat_task(),
                        // pressed: only keys the OS itself would repeat get a
                        // repeat task. Repeating Ctrl/Shift/Alt/... makes the
                        // receiving application see phantom modifier presses.
                        _ if is_repeatable_linux_key(key) => self.spawn_repeat_task(key).await,
                        _ => self.kill_repeat_task(),
                    }
                    key_event(key, state)
                }
                KeyboardEvent::Modifiers { .. } => {}
            },
        }
        // FIXME
        Ok(())
    }

    async fn create(&mut self, _handle: EmulationHandle) {
        // A new session starts with no hand-off in flight.
        self.edge_return_latch.clear();
    }

    async fn destroy(&mut self, _handle: EmulationHandle) {
        // The session ended (leave, disconnect or watchdog timeout): the
        // hand-off latch must not leak into the next session and a held key
        // must not keep repeating.
        self.edge_return_latch.clear();
        self.kill_repeat_task();
    }

    async fn terminate(&mut self) {
        self.edge_return_latch.clear();
        self.kill_repeat_task();
    }

    fn set_edge_return_notifier(&mut self, notifier: EdgeReturnNotifier) {
        self.edge_return = Some(notifier);
    }

    async fn begin_remote_control(&mut self, edge: PointerEdge, along: Option<f32>) {
        self.remote_edge = Some(edge);
        self.edge_return_latch.clear();
        // Fractional motion from the previous session must not leak into the
        // new entry position.
        self.subpixel = Default::default();
        // Park the cursor at the crossing coordinate the peer sent (or at the
        // previous cursor position for a legacy peer). Without an anchor the
        // cursor keeps whatever position it had before the peer entered:
        // sitting exactly on the edge it hands control back on the first pixel
        // of jitter, parked elsewhere the first deltas clamp at another edge
        // and the pointer looks frozen until the user pulls the other way.
        match unsafe { virtual_screen() } {
            Some((screen, monitors)) => {
                let (x, y) = match along {
                    Some(along) => {
                        entry_anchor_fraction_in_monitors(&screen, &monitors, edge, along, 1)
                    }
                    None => {
                        let along = match cursor_pos() {
                            Some(point)
                                if matches!(edge, PointerEdge::Left | PointerEdge::Right) =>
                            {
                                point.y
                            }
                            Some(point) => point.x,
                            None => 0,
                        };
                        entry_anchor(&screen, edge, along, ENTRY_ANCHOR_MARGIN)
                    }
                };
                unsafe {
                    let _ = SetCursorPos(x, y);
                }
                log::info!(
                    "absolute pointer armed at ({x},{y}) on the {edge:?} entry edge (along {along:?})"
                );
            }
            None => log::debug!(
                "could not enumerate the virtual screen to park the pointer on the {edge:?} entry edge"
            ),
        }
    }

    async fn end_remote_control(&mut self) {
        self.remote_edge = None;
        self.edge_return_latch.clear();
    }

    /// Windows synthesizes key repeat in [`Self::spawn_repeat_task`].
    fn synthesizes_key_repeat(&self) -> bool {
        true
    }
}

impl WindowsEmulation {
    /// Report that the remote pointer was pushed past `edge`.
    ///
    /// Requests are throttled and bounded per contiguous edge contact (see
    /// [`EdgeReturnLatch`]): a lost `Leave` is retried a few times, but a peer
    /// that keeps pushing at the edge cannot generate an unbounded stream of
    /// hand-off requests.
    fn request_edge_return(&mut self, edge: PointerEdge, along: Option<f32>) {
        let Some(notifier) = &self.edge_return else {
            return;
        };
        let Some(attempt) = self.edge_return_latch.request(edge, Instant::now()) else {
            return;
        };
        if notifier.send((edge, along)).is_ok() {
            log::debug!(
                "remote pointer pushed past the {edge:?} edge: requesting hand-off \
                 (attempt {attempt})"
            );
        }
    }

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

/// Warning latch for refused `SendInput` calls: UIPI makes the refusal
/// permanent for the current foreground integrity level, so retrying every
/// event would only spam the log.
static SEND_INPUT_WARNED: AtomicBool = AtomicBool::new(false);

fn send_input_safe(input: INPUT) {
    // Never busy-loop here. `SendInput` returns 0 when the foreground window
    // runs at a higher integrity level (UIPI) or the input desktop rejects the
    // event; looping forever used to freeze the whole single-threaded
    // `LocalSet` (capture, network, IPC and watchdog) while burning CPU.
    let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    if sent > 0 {
        // Only successful submissions count as injected input: the capture
        // side uses this marker to avoid mistaking remote-controlled motion
        // for a local barrier crossing.
        input_event::injection::mark();
        return;
    }
    if !SEND_INPUT_WARNED.swap(true, Ordering::Relaxed) {
        log::warn!(
            "SendInput was refused (an elevated foreground window?);              input events are dropped until injection works again"
        );
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
type MonitorRect = (i32, i32, i32, i32);
type MonitorRects = Vec<MonitorRect>;

unsafe fn virtual_screen() -> Option<(VirtualScreen, MonitorRects)> {
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
    let Some(screen) = VirtualScreen::from_monitors(&monitors) else {
        warn_once(
            &SCREEN_ENUM_WARNED,
            "windows: could not enumerate the display monitors, \
             falling back to relative mouse movement",
        );
        return None;
    };
    Some((screen, monitors))
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

/// Whether the OS itself would auto-repeat this key while it is held.
///
/// Modifier and lock keys do not auto-repeat on a real keyboard (holding Ctrl
/// produces no repeated VK_CONTROL), so the repeat task must not synthesize
/// them.
fn is_repeatable_linux_key(key: u32) -> bool {
    !matches!(
        scancode::Linux::try_from(key),
        Ok(scancode::Linux::KeyLeftShift
            | scancode::Linux::KeyRightShift
            | scancode::Linux::KeyLeftCtrl
            | scancode::Linux::KeyRightCtrl
            | scancode::Linux::KeyLeftAlt
            | scancode::Linux::KeyRightalt
            | scancode::Linux::KeyLeftMeta
            | scancode::Linux::KeyRightmeta
            | scancode::Linux::KeyCapsLock
            | scancode::Linux::KeyNumlock
            | scancode::Linux::KeyScrollLock)
    )
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
