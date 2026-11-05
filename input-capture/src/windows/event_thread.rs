use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ptr::addr_of_mut;

use std::default::Default;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS,
    EnumDisplayDevicesW, EnumDisplaySettingsW, MONITOR_DEFAULTTONEAREST, MonitorFromPoint,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW,
    EDD_GET_DEVICE_INTERFACE_NAME, GetCursorPos, GetMessageW, HOOKPROC, HWND_MESSAGE,
    KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PostThreadMessageW,
    RegisterClassW, SetCursorPos, SetWindowsHookExW, TranslateMessage, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WINDOW_STYLE, WM_DISPLAYCHANGE, WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW,
    WNDPROC,
};
use windows::core::{PCWSTR, w};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    scancode::{self, Linux},
};

use super::{CaptureEvent, Position, display_util};

/// Ignore barrier crossings for a moment after a capture was released, so the
/// hand-off does not immediately bounce back to the peer we just left.
const RELEASE_COOLDOWN: Duration = Duration::from_millis(350);
/// Ignore barrier crossings for this long after lan-mouse injected input: the
/// remote peer is (just) driving this machine and its motion must not be
/// mistaken for the local user crossing an edge.
const REMOTE_GRACE_MS: u64 = 120;

pub(crate) struct EventThread {
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    thread: Option<thread::JoinHandle<()>>,
    thread_id: u32,
}

impl EventThread {
    pub(crate) fn new(event_tx: Sender<(Position, CaptureEvent)>) -> Self {
        let request_buffer = Default::default();
        let (thread, thread_id) = start(event_tx, Arc::clone(&request_buffer));
        Self {
            request_buffer,
            thread: Some(thread),
            thread_id,
        }
    }

    pub(crate) fn release_capture(&self) {
        self.signal(RequestType::Release);
    }

    /// Tell the hook thread that a remote peer is currently controlling this
    /// machine. Barrier detection stays disabled while this is true: the
    /// remote side's injected pointer motion otherwise looks like the local
    /// user crossing an edge, which makes the two machines hand control back
    /// and forth ("ping-pong") at the shared border.
    pub(crate) fn set_remote_active(&self, active: bool) {
        self.client_update(ClientUpdate::RemoteActive(active));
    }

    pub(crate) fn create(&self, pos: Position) {
        self.client_update(ClientUpdate::Create(pos));
    }

    pub(crate) fn destroy(&self, pos: Position) {
        self.client_update(ClientUpdate::Destroy(pos));
    }

    fn exit(&self) {
        self.signal(RequestType::Exit);
    }

    fn client_update(&self, request: ClientUpdate) {
        {
            let mut requests = self.request_buffer.lock().unwrap();
            requests.push(request);
        }
        self.signal(RequestType::ClientUpdate);
    }

    fn signal(&self, event_type: RequestType) {
        let id = self.thread_id;
        unsafe { PostThreadMessageW(id, WM_USER, WPARAM(event_type as usize), LPARAM(0)).unwrap() };
    }
}

impl Drop for EventThread {
    fn drop(&mut self) {
        self.exit();
        let _ = self.thread.take().expect("thread").join();
    }
}

enum RequestType {
    ClientUpdate = 0,
    Release = 1,
    Exit = 2,
}

enum ClientUpdate {
    Create(Position),
    Destroy(Position),
    RemoteActive(bool),
}

fn try_send_event(
    pos: Position,
    event: CaptureEvent,
) -> Result<(), TrySendError<(Position, CaptureEvent)>> {
    EVENT_TX.with_borrow_mut(|tx| tx.as_mut().unwrap().try_send((pos, event)))
}

thread_local! {
    /// all configured clients
    static CLIENTS: RefCell<HashSet<Position>> = RefCell::new(HashSet::new());
    /// currently active client
    static ACTIVE_CLIENT: Cell<Option<Position>> = const { Cell::new(None) };
    /// input event channel
    static EVENT_TX: RefCell<Option<Sender<(Position, CaptureEvent)>>> = const { RefCell::new(None) };
    /// position of barrier entry
    static ENTRY_POINT: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    /// physical pixels per logical pixel on the entry monitor (see
    /// [`monitor_scale`]); the wire carries logical pixels so peers with a
    /// different display scale move the cursor at the same logical speed.
    static DPI_SCALE: Cell<f64> = const { Cell::new(1.0) };
    /// previous mouse position (out-of-bounds fallback detector only)
    static PREV_POS: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
    /// displays and generation counter
    static DISPLAYS: RefCell<(Vec<RECT>, i32)> = const { RefCell::new((Vec::new(), 0)) };
    /// whether raw input (WM_INPUT) was registered successfully
    static RAW_INPUT_OK: Cell<bool> = const { Cell::new(false) };
    /// time of the last capture release, used for the re-entry cooldown
    static LAST_RELEASE: Cell<Option<Instant>> = const { Cell::new(None) };
    /// a remote peer is currently driving this machine; local barrier
    /// detection must stay quiet to avoid a hand-off ping-pong
    static REMOTE_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

fn get_msg() -> Option<MSG> {
    unsafe {
        let mut msg = std::mem::zeroed();
        let ret = GetMessageW(addr_of_mut!(msg), None, 0, 0);
        match ret.0 {
            0 => None,
            x if x > 0 => Some(msg),
            _ => panic!("error in GetMessageW"),
        }
    }
}

fn start(
    event_tx: Sender<(Position, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
) -> (thread::JoinHandle<()>, u32) {
    /* condition variable to wait for thead id */
    let thread_id = Arc::new((Condvar::new(), Mutex::new(None)));
    let thread_id_ = Arc::clone(&thread_id);

    let msg_thread = thread::spawn(|| start_routine(thread_id_, event_tx, request_buffer));

    /* wait for thread to set its id */
    let (cond, thread_id) = &*thread_id;
    let mut thread_id = thread_id.lock().unwrap();
    while (*thread_id).is_none() {
        thread_id = cond.wait(thread_id).expect("channel closed");
    }
    (msg_thread, thread_id.expect("thread id"))
}

fn start_routine(
    ready: Arc<(Condvar, Mutex<Option<u32>>)>,
    event_tx: Sender<(Position, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
) {
    EVENT_TX.replace(Some(event_tx));
    /* communicate thread id */
    {
        let (cnd, mtx) = &*ready;
        let mut ready = mtx.lock().unwrap();
        *ready = Some(unsafe { GetCurrentThreadId() });
        cnd.notify_one();
    }

    // Without this the coordinates seen by the low level hooks are
    // virtualized by DPI scaling, which shifts every delta when the two
    // machines use different scale factors.
    unsafe {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
        };
        // The return value is the previous context (null when the thread had
        // none), not a success flag: keep the call unconditional.
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let mouse_proc: HOOKPROC = Some(mouse_proc);
    let kybrd_proc: HOOKPROC = Some(kybrd_proc);
    let window_proc: WNDPROC = Some(window_proc);

    /* register hooks */
    unsafe {
        if let Err(e) = SetWindowsHookExW(WH_MOUSE_LL, mouse_proc, None, 0) {
            log::error!("could not install the low level mouse hook: {e}");
        }
        if let Err(e) = SetWindowsHookExW(WH_KEYBOARD_LL, kybrd_proc, None, 0) {
            log::error!("could not install the low level keyboard hook: {e}");
        }
    }

    let Ok(instance) = (unsafe { GetModuleHandleW(None) }) else {
        log::error!("could not get the module handle");
        return;
    };
    let instance = instance.into();
    let window_class: WNDCLASSW = WNDCLASSW {
        lpfnWndProc: window_proc,
        hInstance: instance,
        lpszClassName: w!("lan-mouse-message-window-class"),
        ..Default::default()
    };

    static WINDOW_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
    if WINDOW_CLASS_REGISTERED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        /* register window class if not yet done so */
        unsafe {
            let ret = RegisterClassW(&window_class);
            if ret == 0 {
                panic!("RegisterClassW");
            }
        }
    }

    /* message-only window: WM_DISPLAYCHANGE + WM_INPUT (raw mouse) */
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("lan-mouse-message-window-class"),
            w!("lan-mouse-msg-window"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
    };
    let Ok(hwnd) = hwnd else {
        log::error!("could not create the capture message window");
        return;
    };

    // Relative motion via raw input is immune to pointer clamping and DPI
    // virtualisation: at the desktop edge Windows stops the *cursor* but
    // still reports the movement that pushed against it. This is what makes
    // barrier detection and in-capture motion identical on Windows 10 and
    // 11 and on scaled displays.
    unsafe {
        let devices = [RAWINPUTDEVICE {
            usUsagePage: 1, // HID_USAGE_PAGE_GENERIC
            usUsage: 2,     // HID_USAGE_GENERIC_MOUSE
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        }];
        match RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32) {
            Ok(()) => RAW_INPUT_OK.set(true),
            Err(e) => log::error!(
                "could not register for raw mouse input ({e}); falling back to cursor deltas"
            ),
        }
    }

    /* run message loop */
    while let Some(msg) = get_msg() {
        // mouse / keybrd proc do not actually return a message
        if msg.hwnd.0.is_null() {
            /* messages sent via PostThreadMessage */
            match msg.wParam.0 {
                x if x == RequestType::Exit as usize => break,
                x if x == RequestType::Release as usize => {
                    release_active_client();
                }
                x if x == RequestType::ClientUpdate as usize => {
                    let requests = {
                        let mut res = vec![];
                        let mut requests = request_buffer.lock().unwrap();
                        for request in requests.drain(..) {
                            res.push(request);
                        }
                        res
                    };

                    for request in requests {
                        update_clients(request)
                    }
                }
                _ => {}
            }
        } else {
            /* other messages for window_procs */
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

fn release_active_client() {
    if ACTIVE_CLIENT.take().is_some() {
        // Cooldown: the pointer is still parked at the edge when control is
        // handed back, so the next outgoing movement must not immediately
        // re-enter the peer ("ping-pong" at the shared border).
        LAST_RELEASE.replace(Some(Instant::now()));
    }
    PREV_POS.replace(None);
}

/// Enter the client registered at `pos`.
///
/// `at` is the cursor position that triggered the crossing, in the physical
/// coordinate space used by the low level hook and the monitor enumeration.
/// The real cursor is pinned to the resulting `ENTRY_POINT`; in raw-input mode
/// it is also kept there on every motion event, so the pointer can neither
/// drift into another monitor nor stop reporting deltas because the OS
/// clamped it to a display edge.
fn activate(pos: Position, at: (i32, i32)) {
    ACTIVE_CLIENT.replace(Some(pos));
    let entry_point = DISPLAYS
        .with_borrow(|(displays, _)| display_util::clamp_to_display_bounds(displays, at, at));
    ENTRY_POINT.replace(entry_point);
    // Physical pixels per logical pixel on the monitor the pointer is pinned
    // to. Raw counts are proportional to physical pixels at the default
    // pointer speed, so dividing by the scale yields logical 96-DPI units
    // that peers with another scale can consume at the same logical speed.
    DPI_SCALE.replace(unsafe { monitor_scale(entry_point) });
    PREV_POS.replace(Some(entry_point));
    unsafe {
        let _ = SetCursorPos(entry_point.0, entry_point.1);
    }
    log::debug!("ENTERED @ {at:?}, pinned to {entry_point:?}");
    // `Begin` must land before any motion from this capture. It is enqueued
    // from the hook thread without blocking; if the channel is momentarily
    // full, undo the activation and let the next raw event retry instead of
    // risking an out-of-order stream (Windows can kill hooks that block).
    if let Err(e) = try_send_event(pos, CaptureEvent::Begin) {
        log::warn!("could not enqueue capture begin: {e}");
        release_active_client();
    }
}

/// Whether a local barrier crossing may start a capture right now.
fn may_activate() -> bool {
    if REMOTE_ACTIVE.get() {
        return false;
    }
    LAST_RELEASE
        .get()
        .is_none_or(|released| released.elapsed() >= RELEASE_COOLDOWN)
}

/// Barrier detection from raw mouse movement while the pointer is clamped to
/// the desktop edge.
///
/// This is the primary detector: it works whether or not Windows ever reports
/// a coordinate outside the monitor bounds, which is exactly where Windows 10
/// and 11 (and scaled displays) behave inconsistently.
unsafe fn try_activate_by_raw(dx: i32, dy: i32) {
    if !may_activate() {
        return;
    }
    // The remote peer's injected motion is visible to raw input as well (and
    // reaches here before the service can flip `REMOTE_ACTIVE`); never treat
    // it as a local barrier crossing.
    if input_event::injection::is_recent(REMOTE_GRACE_MS) {
        return;
    }
    let mut cursor = POINT::default();
    if unsafe { GetCursorPos(&mut cursor) }.is_err() {
        return;
    }
    let entered = DISPLAYS.with_borrow_mut(|(displays, generation)| {
        update_display_regions(displays, generation);
        display_util::outward_edge_crossing((cursor.x, cursor.y), (dx, dy), displays)
    });
    let Some(pos) = entered else {
        return;
    };
    if !CLIENTS.with_borrow(|clients| clients.contains(&pos)) {
        return;
    }
    activate(pos, (cursor.x, cursor.y));
}

/// Fallback detector for machines where raw input could not be registered:
/// the old "cursor left the monitor bounds" check.
unsafe fn try_activate_by_legacy(lparam: LPARAM) {
    if !may_activate() {
        return;
    }
    if input_event::injection::is_recent(REMOTE_GRACE_MS) {
        return;
    }
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    if mouse_low_level.flags & LLMHF_INJECTED != 0 {
        // Injected movement (e.g. another tool, or the remote peer when this
        // machine is also configured to send) must not start a local capture.
        return;
    }
    let curr_pos = (mouse_low_level.pt.x, mouse_low_level.pt.y);
    let prev_pos = PREV_POS.get().unwrap_or(curr_pos);
    PREV_POS.replace(Some(curr_pos));
    let entered = DISPLAYS.with_borrow_mut(|(displays, generation)| {
        update_display_regions(displays, generation);
        display_util::entered_barrier(prev_pos, curr_pos, displays)
    });
    let Some(pos) = entered else {
        return;
    };
    if !CLIENTS.with_borrow(|clients| clients.contains(&pos)) {
        return;
    }
    activate(pos, curr_pos);
}

/// Decode one `WM_INPUT` packet.
///
/// Idle: feed barrier detection with the real (unclamped) raw delta.
/// Active: forward the delta to the peer, scaled to logical 96-DPI units.
unsafe fn process_raw_input(lparam: LPARAM) {
    let hraw = HRAWINPUT(lparam.0 as *mut std::ffi::c_void);
    let header_size = std::mem::size_of::<RAWINPUTHEADER>() as u32;
    let mut size = 0u32;
    if unsafe { GetRawInputData(hraw, RID_INPUT, None, &mut size, header_size) } == u32::MAX {
        return;
    }
    if size as usize > std::mem::size_of::<RAWINPUT>() {
        log::debug!("ignoring oversized raw input packet ({size} bytes)");
        return;
    }
    let mut raw = RAWINPUT::default();
    let mut written = size;
    if unsafe {
        GetRawInputData(
            hraw,
            RID_INPUT,
            Some(&mut raw as *mut _ as *mut std::ffi::c_void),
            &mut written,
            header_size,
        )
    } == u32::MAX
    {
        return;
    }
    if raw.header.dwType != RIM_TYPEMOUSE.0 {
        return;
    }
    let mouse = unsafe { raw.data.mouse };
    // Absolute pointing devices (tablets, some remote desktop clients) do not
    // report relative counts; adding them as deltas would jump the pointer.
    if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0 {
        return;
    }
    let (dx, dy) = (mouse.lLastX, mouse.lLastY);
    if dx == 0 && dy == 0 {
        return;
    }
    match ACTIVE_CLIENT.get() {
        Some(pos) => {
            let scale = DPI_SCALE.get().max(0.1);
            let _ = try_send_event(
                pos,
                CaptureEvent::Input(Event::Pointer(PointerEvent::Motion {
                    time: 0,
                    dx: dx as f64 / scale,
                    dy: dy as f64 / scale,
                })),
            );
            // The low level hook does not freeze the pointer; park it on the
            // entry point after every movement so it cannot drift.
            let (ex, ey) = ENTRY_POINT.get();
            unsafe {
                let _ = SetCursorPos(ex, ey);
            }
        }
        None => unsafe { try_activate_by_raw(dx, dy) },
    }
}

unsafe extern "system" fn mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Required by the hook contract: negative codes must be passed on and the
    // return value of CallNextHookEx propagated.
    if ncode < 0 {
        return unsafe { CallNextHookEx(None, ncode, wparam, lparam) };
    }

    let raw_input = RAW_INPUT_OK.get();
    if ACTIVE_CLIENT.get().is_none() {
        // Primary detector: raw input (works with a clamped pointer).
        // Fallback only if raw input could not be registered.
        if !raw_input && wparam.0 == WM_MOUSEMOVE as usize {
            unsafe { try_activate_by_legacy(lparam) };
        }
        return unsafe { CallNextHookEx(None, ncode, wparam, lparam) };
    }

    let Some(pos) = ACTIVE_CLIENT.get() else {
        return LRESULT(1);
    };

    // Motion is forwarded from raw input; only block it from local apps here
    // (forwarding it too would double every movement).
    if wparam.0 == WM_MOUSEMOVE as usize {
        return LRESULT(1);
    }

    let Some(pointer_event) = to_mouse_event(wparam, lparam) else {
        return LRESULT(1);
    };
    if let Err(e) = try_send_event(pos, CaptureEvent::Input(Event::Pointer(pointer_event))) {
        log::debug!("dropping capture event: {e}");
    }
    LRESULT(1)
}

unsafe extern "system" fn kybrd_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    /* get active client if any */
    let Some(client) = ACTIVE_CLIENT.get() else {
        return CallNextHookEx(None, ncode, wparam, lparam);
    };

    /* convert to key event */
    let Some(key_event) = to_key_event(wparam, lparam) else {
        return LRESULT(1);
    };

    if let Err(e) = try_send_event(client, CaptureEvent::Input(Event::Keyboard(key_event))) {
        log::warn!("e: {e}");
    }

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    uint: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match uint {
        WM_DISPLAYCHANGE => {
            log::debug!("display resolution changed");
            DISPLAY_RESOLUTION_GENERATION.fetch_add(1, Ordering::Release);
            LRESULT(0)
        }
        WM_INPUT => {
            unsafe { process_raw_input(lparam) };
            // The system must be allowed to perform its raw-input cleanup.
            unsafe { DefWindowProcW(hwnd, uint, wparam, lparam) }
        }
        _ => unsafe { DefWindowProcW(hwnd, uint, wparam, lparam) },
    }
}

static DISPLAY_RESOLUTION_GENERATION: AtomicI32 = AtomicI32::new(1);

fn update_display_regions(displays: &mut Vec<RECT>, generation: &mut i32) {
    let global_generation = DISPLAY_RESOLUTION_GENERATION.load(Ordering::Acquire);
    if *generation != global_generation {
        enumerate_displays(displays);
        log::debug!("displays: {displays:?}");
        *generation = global_generation;
    }
}

fn enumerate_displays(display_rects: &mut Vec<RECT>) {
    display_rects.clear();
    unsafe {
        let mut devices = vec![];
        for i in 0.. {
            let mut device: DISPLAY_DEVICEW = std::mem::zeroed();
            device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            let ret = EnumDisplayDevicesW(None, i, &mut device, EDD_GET_DEVICE_INTERFACE_NAME);
            if ret == FALSE {
                break;
            }
            if device
                .StateFlags
                .contains(DISPLAY_DEVICE_ATTACHED_TO_DESKTOP)
            {
                devices.push(device.DeviceName);
            }
        }
        for device in devices {
            let mut dev_mode: DEVMODEW = std::mem::zeroed();
            dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            let ret = EnumDisplaySettingsW(
                PCWSTR::from_raw(&device as *const _),
                ENUM_CURRENT_SETTINGS,
                &mut dev_mode,
            );
            if ret == FALSE {
                log::warn!("no display mode");
            }

            let pos = dev_mode.Anonymous1.Anonymous2.dmPosition;
            let (x, y) = (pos.x, pos.y);
            let (width, height) = (dev_mode.dmPelsWidth, dev_mode.dmPelsHeight);

            display_rects.push(RECT {
                left: x,
                right: x + width as i32,
                top: y,
                bottom: y + height as i32,
            });
        }
    }
}

/// Physical pixels per logical (96 DPI) pixel on the monitor containing
/// `point`.
///
/// `GetDpiForMonitor` reports the real scale only for a per-monitor DPI aware
/// process; `main` sets that up on Windows, so this matches the physical
/// coordinates delivered by the low level hook.
unsafe fn monitor_scale(point: (i32, i32)) -> f64 {
    let monitor = unsafe {
        MonitorFromPoint(
            POINT {
                x: point.0,
                y: point.1,
            },
            MONITOR_DEFAULTTONEAREST,
        )
    };
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

fn update_clients(request: ClientUpdate) {
    match request {
        ClientUpdate::Create(pos) => {
            CLIENTS.with_borrow_mut(|clients| clients.insert(pos));
        }
        ClientUpdate::Destroy(pos) => {
            if ACTIVE_CLIENT.get() == Some(pos) {
                release_active_client();
            }
            CLIENTS.with_borrow_mut(|clients| clients.remove(&pos));
        }
        ClientUpdate::RemoteActive(active) => {
            REMOTE_ACTIVE.set(active);
            log::debug!("remote peer control active: {active}");
        }
    }
}

fn to_key_event(wparam: WPARAM, lparam: LPARAM) -> Option<KeyboardEvent> {
    let kybrdllhookstruct: KBDLLHOOKSTRUCT = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let mut scan_code = kybrdllhookstruct.scanCode;
    log::trace!("scan_code: {scan_code}");
    if kybrdllhookstruct.flags.contains(LLKHF_EXTENDED) {
        scan_code |= 0xE000;
    }
    let Ok(win_scan_code) = scancode::Windows::try_from(scan_code) else {
        log::warn!("failed to translate to windows scancode: {scan_code}");
        return None;
    };
    log::trace!("windows_scan: {win_scan_code:?}");
    let Ok(linux_scan_code): Result<Linux, ()> = win_scan_code.try_into() else {
        log::warn!("failed to translate into linux scancode: {win_scan_code:?}");
        return None;
    };
    log::trace!("windows_scan: {linux_scan_code:?}");
    let scan_code = linux_scan_code as u32;
    match wparam {
        WPARAM(p) if p == WM_KEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_KEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        WPARAM(p) if p == WM_SYSKEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_SYSKEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        _ => None,
    }
}

fn to_mouse_event(wparam: WPARAM, lparam: LPARAM) -> Option<PointerEvent> {
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    match wparam {
        WPARAM(p) if p == WM_LBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 1,
        }),
        WPARAM(p) if p == WM_MBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 1,
        }),
        WPARAM(p) if p == WM_RBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 1,
        }),
        WPARAM(p) if p == WM_LBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 0,
        }),
        WPARAM(p) if p == WM_RBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 0,
        }),
        // Motion is produced from raw input (`process_raw_input`): the hook's
        // `pt` is the clamped cursor position, not the movement, and using it
        // as a delta from the entry point made the remote pointer freeze as
        // soon as the local cursor hit the screen edge.
        WPARAM(p) if p == WM_MOUSEMOVE as usize => None,
        WPARAM(p) if p == WM_MOUSEWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 0,
            value: -(mouse_low_level.mouseData as i32 >> 16),
        }),
        WPARAM(p) if p == WM_XBUTTONDOWN as usize || p == WM_XBUTTONUP as usize => {
            let hb = mouse_low_level.mouseData >> 16;
            let button = match hb {
                1 => BTN_BACK,
                2 => BTN_FORWARD,
                _ => {
                    log::warn!("unknown mouse button");
                    return None;
                }
            };
            Some(PointerEvent::Button {
                time: 0,
                button,
                state: if p == WM_XBUTTONDOWN as usize { 1 } else { 0 },
            })
        }
        WPARAM(p) if p == WM_MOUSEHWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 1, // Horizontal
            value: mouse_low_level.mouseData as i32 >> 16,
        }),
        w => {
            log::warn!("unknown mouse event: {w:?}");
            None
        }
    }
}
