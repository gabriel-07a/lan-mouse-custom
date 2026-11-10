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
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromPoint,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, MOUSE_VIRTUAL_DESKTOP, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEMOUSE,
    RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetMessageW, HOOKPROC, HWND_MESSAGE, HWND_TOPMOST, KBDLLHOOKSTRUCT, LLKHF_EXTENDED,
    LLKHF_INJECTED, LLMHF_INJECTED, MA_NOACTIVATE, MONITORINFOF_PRIMARY, MSG, MSLLHOOKSTRUCT,
    PM_NOREMOVE, PeekMessageW, PostThreadMessageW, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, SetCursor, SetCursorPos, SetWindowPos, SetWindowsHookExW,
    ShowWindow, TranslateMessage, WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_STYLE, WM_DISPLAYCHANGE,
    WM_DPICHANGED, WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SETCURSOR, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP,
    WNDCLASSW, WNDPROC, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{BOOL, w};

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
    pub(crate) fn new(
        event_tx: Sender<(Position, CaptureEvent)>,
    ) -> Result<Self, crate::CaptureCreationError> {
        let request_buffer = Default::default();
        let (thread, thread_id) = start(event_tx, Arc::clone(&request_buffer))?;
        Ok(Self {
            request_buffer,
            thread: Some(thread),
            thread_id,
        })
    }

    pub(crate) fn release_capture(&self) {
        self.signal(RequestType::Release);
    }

    /// Tell the hook thread which edge a remote peer entered through and is
    /// currently controlling this machine from.
    ///
    /// Injected pointer motion must not look like the local user crossing an
    /// edge, otherwise the two machines hand control back and forth
    /// ("ping-pong") at the shared border. The one exception is the edge the
    /// peer entered through: a crossing there is the return hand-off and must
    /// release the peer's capture so the cursor can come back.
    pub(crate) fn set_remote_peer(&self, peer: Option<Position>) {
        self.client_update(ClientUpdate::RemotePeer(peer));
    }

    pub(crate) fn create(&self, pos: Position) {
        self.client_update(ClientUpdate::Create(pos));
    }

    pub(crate) fn destroy(&self, pos: Position) {
        self.client_update(ClientUpdate::Destroy(pos));
    }

    /// Ask the capture thread to exit. Returns whether the message could be
    /// delivered (the thread may already be gone).
    fn exit(&self) -> bool {
        self.signal(RequestType::Exit)
    }

    fn client_update(&self, request: ClientUpdate) {
        {
            let mut requests = self.request_buffer.lock().expect("request buffer");
            requests.push(request);
        }
        self.signal(RequestType::ClientUpdate);
    }

    fn signal(&self, event_type: RequestType) -> bool {
        // A post to a thread that has no message queue (or already exited) fails
        // with ERROR_INVALID_THREAD_ID. The thread id is only published after
        // the queue exists, so this is only a shutdown race; give up and log
        // rather than sleeping/looping forever or panicking: the release
        // profile aborts on panics, and this path runs during teardown.
        if self
            .thread
            .as_ref()
            .is_none_or(|thread| thread.is_finished())
        {
            return false;
        }
        let request = event_type as usize;
        for _ in 0..50 {
            let posted =
                unsafe { PostThreadMessageW(self.thread_id, WM_USER, WPARAM(request), LPARAM(0)) };
            if posted.is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        log::error!(
            "could not post a message to the capture thread (id {})",
            self.thread_id
        );
        false
    }
}

impl Drop for EventThread {
    fn drop(&mut self) {
        // Post the Exit message BEFORE taking the handle: `signal()` consults
        // `self.thread` to avoid posting into a thread that is already gone,
        // so taking the handle first made the post fail unconditionally (the
        // thread was then always detached and leaked).
        let notified = self.exit();
        let Some(thread) = self.thread.take() else {
            return;
        };
        if notified {
            let _ = thread.join();
        } else if thread.is_finished() {
            // Nothing to do: the thread already returned, just reap it.
            let _ = thread.join();
        } else {
            // Never block shutdown on a thread that cannot be reached: it is
            // wedged before its message loop (which is why the id was not
            // published yet). Detaching leaks it, but hanging the app is worse.
            log::error!("capture thread did not acknowledge shutdown; detaching it");
        }
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
    RemotePeer(Option<Position>),
}

fn try_send_event(
    pos: Position,
    event: CaptureEvent,
) -> Result<(), TrySendError<(Position, CaptureEvent)>> {
    EVENT_TX.with_borrow_mut(|tx| tx.as_mut().unwrap().try_send((pos, event)))
}

fn hidden_pointer_hwnd() -> Option<HWND> {
    match HIDDEN_POINTER_WINDOW.get() {
        0 => None,
        raw => Some(HWND(raw as *mut core::ffi::c_void)),
    }
}

/// Window procedure for the hidden-pointer overlay.
///
/// The overlay is a 1x1 topmost window positioned exactly under the pinned
/// pointer. Returning `TRUE` from `WM_SETCURSOR` after `SetCursor(NULL)`
/// removes the cursor for as long as the pointer stays over that window, which
/// is precisely the duration of the capture.
unsafe extern "system" fn hidden_pointer_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_SETCURSOR => {
            unsafe {
                SetCursor(None);
            }
            LRESULT(1)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Park the overlay on `point` and show it, hiding the local pointer.
fn show_hidden_pointer(point: (i32, i32)) {
    let Some(hwnd) = hidden_pointer_hwnd() else {
        return;
    };
    let positioned = unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            point.0,
            point.1,
            1,
            1,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    };
    if let Err(e) = positioned {
        if !HIDDEN_POINTER_WARNED.swap(true, Ordering::Relaxed) {
            log::warn!("could not position the local cursor overlay: {e}");
        }
        return;
    }
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        // Showing a window under a stationary cursor does not reliably send
        // WM_SETCURSOR; remove the cursor explicitly and let the window
        // procedure keep it removed for the rest of the capture.
        SetCursor(None);
    }
    if !HIDDEN_POINTER_SHOWN.replace(true) {
        log::debug!("local pointer hidden while the peer is controlled");
    }
}

/// Hide the overlay, restoring the normal cursor.
fn hide_hidden_pointer() {
    let Some(hwnd) = hidden_pointer_hwnd() else {
        return;
    };
    if HIDDEN_POINTER_SHOWN.replace(false) {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        log::debug!("local pointer restored");
    }
}

fn destroy_hidden_pointer() {
    hide_hidden_pointer();
    if let Some(hwnd) = hidden_pointer_hwnd() {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        HIDDEN_POINTER_WINDOW.set(0);
    }
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
    /// previous absolute raw position of an absolute pointing device (tablet,
    /// digitizer, some remote-desktop sessions), converted to deltas
    static PREV_ABS_POS: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
    /// display rectangles, generation counter and the primary monitor rect
    /// (needed to map absolute input: MOUSE_VIRTUAL_DESKTOP selects the whole
    /// virtual desktop vs. the primary monitor)
    static DISPLAYS: RefCell<(Vec<RECT>, i32, Option<RECT>)> =
        const { RefCell::new((Vec::new(), 0, None)) };
    /// whether raw input (WM_INPUT) was registered successfully
    static RAW_INPUT_OK: Cell<bool> = const { Cell::new(false) };
    /// time of the last capture release, used for the re-entry cooldown
    static LAST_RELEASE: Cell<Option<Instant>> = const { Cell::new(None) };
    /// edge a remote peer entered through while it drives this machine
    /// (`None` = no peer is driving us); local barrier detection must stay
    /// quiet except at that edge, where a crossing is the return hand-off
    static REMOTE_PEER: Cell<Option<Position>> = const { Cell::new(None) };
    /// 1x1 topmost window used to hide the local pointer while capturing.
    /// 0 means "not created" (raw pointer value of the HWND).
    static HIDDEN_POINTER_WINDOW: Cell<isize> = const { Cell::new(0) };
    /// whether the hidden-pointer overlay is currently shown
    static HIDDEN_POINTER_SHOWN: Cell<bool> = const { Cell::new(false) };
}

/// Log a failure to set up the local pointer overlay only once.
static HIDDEN_POINTER_WARNED: AtomicBool = AtomicBool::new(false);

fn get_msg() -> Option<MSG> {
    unsafe {
        let mut msg = std::mem::zeroed();
        let ret = GetMessageW(addr_of_mut!(msg), None, 0, 0);
        match ret.0 {
            0 => None,
            x if x > 0 => Some(msg),
            // GetMessageW returns -1 on error. Treat it like WM_QUIT: the
            // message loop cannot make progress anymore. Panicking here would
            // abort the process (release profile uses panic = "abort").
            _ => {
                log::error!("GetMessageW failed: {}", std::io::Error::last_os_error());
                None
            }
        }
    }
}

/// Handshake slot between the capture thread and its creator: the thread id is
/// only published once the thread owns a message queue, so a request posted by
/// [`EventThread::signal`] can never race the queue creation (that race made
/// `PostThreadMessageW` fail and, previously, the process abort).
type Handshake = Arc<(Condvar, Mutex<Option<Result<u32, String>>>)>;

fn announce_ready(handshake: &Handshake, result: Result<u32, String>) {
    let (cond, slot) = &**handshake;
    *slot.lock().expect("capture handshake lock") = Some(result);
    cond.notify_one();
}

fn start(
    event_tx: Sender<(Position, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
) -> Result<(thread::JoinHandle<()>, u32), crate::CaptureCreationError> {
    let handshake: Handshake = Arc::new((Condvar::new(), Mutex::new(None)));
    let handshake_thread = Arc::clone(&handshake);

    let msg_thread = thread::Builder::new()
        .name("lan-mouse-capture".to_owned())
        .spawn(move || start_routine(handshake_thread, event_tx, request_buffer))
        .map_err(|e| {
            log::error!("could not spawn the windows capture thread: {e}");
            crate::CaptureCreationError::Windows
        })?;

    let (cond, slot) = &*handshake;
    let mut slot = slot.lock().expect("capture handshake lock");
    while slot.is_none() {
        slot = cond.wait(slot).expect("capture handshake wait");
    }
    let result = slot.take().expect("capture handshake result");
    drop(slot);

    match result {
        Ok(thread_id) => Ok((msg_thread, thread_id)),
        Err(e) => {
            log::error!("could not start the windows capture thread: {e}");
            let _ = msg_thread.join();
            Err(crate::CaptureCreationError::Windows)
        }
    }
}

fn start_routine(
    handshake: Handshake,
    event_tx: Sender<(Position, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
) {
    EVENT_TX.replace(Some(event_tx));

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

    // This thread owns the low level hooks and the RAWINPUT message pump. If a
    // CPU hog (game, build, browser) saturates the machine while it runs at
    // normal priority, the scheduler can delay the delivery of a mouse packet
    // by a full quantum (~15 ms), which is felt as input lag. Raising the
    // priority does not bypass the USB/ISR path, but it keeps the hook thread
    // runnable as soon as the input is queued, which is what remote-desktop
    // implementations do for their capture threads.
    unsafe {
        if let Err(e) = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) {
            log::warn!("could not raise the capture thread priority: {e}");
        }
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

    let instance = match unsafe { GetModuleHandleW(None) } {
        Ok(instance) => instance,
        Err(e) => {
            return announce_ready(&handshake, Err(format!("GetModuleHandleW failed: {e}")));
        }
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
                // Allow a later backend creation (e.g. after the user
                // reenables capture) to retry instead of permanently
                // believing the class exists.
                WINDOW_CLASS_REGISTERED.store(false, Ordering::SeqCst);
                return announce_ready(
                    &handshake,
                    Err(format!(
                        "RegisterClassW failed: {}",
                        std::io::Error::last_os_error()
                    )),
                );
            }
        }
    }

    /* message-only window: receives the raw mouse input (WM_INPUT) */
    let hwnd = match unsafe {
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
    } {
        Ok(hwnd) => hwnd,
        Err(e) => {
            return announce_ready(
                &handshake,
                Err(format!("could not create the capture message window: {e}")),
            );
        }
    };

    // A message-only window does not receive broadcast messages, so
    // WM_DISPLAYCHANGE/WM_DPICHANGED never reached the capture thread. This
    // hidden top-level window exists only to receive them; raw input stays on
    // the message-only window above. It is never shown, so it has no taskbar
    // entry, and it is destroyed together with the thread.
    let _display_window = match unsafe {
        CreateWindowExW(
            Default::default(),
            w!("lan-mouse-message-window-class"),
            w!("lan-mouse-display-window"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
    } {
        Ok(hwnd) => Some(hwnd),
        Err(e) => {
            log::warn!(
                "could not create the display-change window ({e}); display hotplug \
                 while capturing will not be picked up until the next capture"
            );
            None
        }
    };

    // 1x1 topmost window that hides the local pointer while a capture is
    // active. The pointer is pinned to the entry point, so this window sits
    // exactly under the hotspot and its WM_SETCURSOR handler removes the
    // cursor. A real window (unlike ShowCursor's global counter) is owned by
    // this thread: it is destroyed with the process, so a crash cannot leave
    // the cursor hidden forever.
    let hidden_pointer_class = WNDCLASSW {
        lpfnWndProc: Some(hidden_pointer_proc),
        hInstance: instance,
        lpszClassName: w!("lan-mouse-hidden-pointer"),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&hidden_pointer_class) } == 0 {
        // The class may already exist when the capture backend is recreated
        // (e.g. after an error). Creating the window can still succeed.
        log::debug!("local cursor overlay class was already registered");
    }
    let hidden = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("lan-mouse-hidden-pointer"),
            w!("lan-mouse-hidden-pointer"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(instance),
            None,
        )
    };
    match hidden {
        Ok(hidden) => HIDDEN_POINTER_WINDOW.set(hidden.0 as isize),
        Err(e) => log::warn!("could not create the local cursor overlay: {e}"),
    }

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

    // Force the thread message queue into existence before publishing the
    // thread id: PostThreadMessageW fails while a thread has no queue, which
    // used to race teardown/release requests and abort release builds.
    unsafe {
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
    }
    announce_ready(&handshake, Ok(unsafe { GetCurrentThreadId() }));

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

    // The overlay must never survive the capture thread: a stale window (or
    // a hidden cursor) could otherwise outlive the capture.
    destroy_hidden_pointer();
}

fn release_active_client() {
    if ACTIVE_CLIENT.take().is_some() {
        // Cooldown: the pointer is still parked at the edge when control is
        // handed back, so the next outgoing movement must not immediately
        // re-enter the peer ("ping-pong" at the shared border).
        LAST_RELEASE.replace(Some(Instant::now()));
    }
    PREV_POS.replace(None);
    hide_hidden_pointer();
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
        .with_borrow(|(displays, _, _)| display_util::clamp_to_display_bounds(displays, at, at));
    ENTRY_POINT.replace(entry_point);
    // Physical pixels per logical pixel on the monitor the pointer is pinned
    // to. Raw counts are proportional to physical pixels at the default
    // pointer speed, so dividing by the scale yields logical 96-DPI units
    // that peers with another scale can consume at the same logical speed.
    DPI_SCALE.replace(unsafe { monitor_scale(entry_point) });
    PREV_POS.replace(Some(entry_point));
    // Show the overlay under the pinned pointer *before* moving the cursor:
    // the resulting WM_SETCURSOR is delivered to the overlay and removes the
    // local cursor for the duration of the capture.
    show_hidden_pointer(entry_point);
    unsafe {
        let _ = SetCursorPos(entry_point.0, entry_point.1);
    }
    log::debug!("ENTERED @ {at:?}, pinned to {entry_point:?}");
    // Carry the exact crossing coordinate along the edge so the peer can
    // continue the pointer from the same relative position.
    let begin = match DISPLAYS
        .with_borrow(|(displays, _, _)| display_util::edge_fraction(displays, pos, at))
    {
        Some(along) => CaptureEvent::BeginAt(along),
        None => CaptureEvent::Begin,
    };
    // `Begin` must land before any motion from this capture. It is enqueued
    // from the hook thread without blocking; if the channel is momentarily
    // full, undo the activation and let the next raw event retry instead of
    // risking an out-of-order stream (Windows can kill hooks that block).
    if let Err(e) = try_send_event(pos, begin) {
        log::warn!("could not enqueue capture begin: {e}");
        release_active_client();
    }
}

/// Whether a local barrier crossing may start a capture right now.
///
/// The remote-active state is handled per edge by [`edge_activation_allowed`]:
/// the cooldown here only prevents an immediate bounce-back after a release.
fn may_activate() -> bool {
    LAST_RELEASE
        .get()
        .is_none_or(|released| released.elapsed() >= RELEASE_COOLDOWN)
}

/// Whether a crossing at `entered` may start a capture while a remote peer is
/// driving this machine.
///
/// Only the edge the peer entered through qualifies: pushing the injected
/// pointer back out that edge is the return hand-off. Any other edge would
/// reflect the injected motion back to the peer (feedback loop).
fn edge_activation_allowed(remote_peer: Option<Position>, entered: Position) -> bool {
    remote_peer.is_none_or(|remote| remote == entered)
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
    let remote_peer = REMOTE_PEER.get();
    // The remote peer's injected motion is visible to raw input as well (and
    // reaches here before the service can flip the remote state); never treat
    // it as a local barrier crossing — except at the edge the peer entered
    // through, where it is the return hand-off and the peer must be released.
    if remote_peer.is_none() && input_event::injection::is_recent(REMOTE_GRACE_MS) {
        return;
    }
    let mut cursor = POINT::default();
    if unsafe { GetCursorPos(&mut cursor) }.is_err() {
        return;
    }
    let entered = DISPLAYS.with_borrow_mut(|(displays, generation, primary)| {
        update_display_regions(displays, generation, primary);
        display_util::outward_edge_crossing((cursor.x, cursor.y), (dx, dy), displays)
    });
    let Some(pos) = entered else {
        return;
    };
    if !edge_activation_allowed(remote_peer, pos) {
        return;
    }
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
    let remote_peer = REMOTE_PEER.get();
    if remote_peer.is_none() && input_event::injection::is_recent(REMOTE_GRACE_MS) {
        return;
    }
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    if mouse_low_level.flags & LLMHF_INJECTED != 0 && remote_peer.is_none() {
        // Injected movement (e.g. another tool, or the remote peer when this
        // machine is also configured to send) must not start a local capture.
        // While a peer is driving us it is the return motion and is allowed at
        // the edge it entered through (checked below).
        return;
    }
    let curr_pos = (mouse_low_level.pt.x, mouse_low_level.pt.y);
    let prev_pos = PREV_POS.get().unwrap_or(curr_pos);
    PREV_POS.replace(Some(curr_pos));
    let entered = DISPLAYS.with_borrow_mut(|(displays, generation, primary)| {
        update_display_regions(displays, generation, primary);
        display_util::entered_barrier(prev_pos, curr_pos, displays)
    });
    let Some(pos) = entered else {
        return;
    };
    if !edge_activation_allowed(remote_peer, pos) {
        return;
    }
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
    // Absolute pointing devices (tablets, some digitizers, some remote
    // desktop sessions) do not report relative counts. Discarding them made
    // the pointer freeze for those devices; consecutive absolute positions are
    // converted into deltas instead.
    if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0 {
        let virtual_desktop = mouse.usFlags.0 & MOUSE_VIRTUAL_DESKTOP.0 != 0;
        let Some(previous) = PREV_ABS_POS.replace(Some((mouse.lLastX, mouse.lLastY))) else {
            // The first packet only establishes the reference position.
            return;
        };
        // RAWMOUSE documentation: with MOUSE_MOVE_ABSOLUTE the coordinates are
        // always normalized to 0..=65535; MOUSE_VIRTUAL_DESKTOP only selects
        // whether they map over the whole virtual desktop or the primary
        // monitor. Use the monitor rectangles (physical pixels) as the extent;
        // GetSystemMetrics would be DPI-virtualized in this per-monitor aware
        // thread and mix up the scales on mixed-DPI layouts.
        let extent = DISPLAYS.with_borrow_mut(|(displays, generation, primary)| {
            update_display_regions(displays, generation, primary);
            display_util::mapping_extent(displays, *primary, virtual_desktop).or_else(|| {
                // Unknown primary monitor (e.g. enumeration raced a hotplug):
                // fall back to the virtual desktop rather than dropping input.
                display_util::mapping_extent(displays, *primary, true)
            })
        });
        let Some(extent) = extent else {
            return;
        };
        let Some((dx, dy)) = display_util::absolute_mouse_delta(
            (mouse.lLastX, mouse.lLastY),
            Some(previous),
            extent,
        ) else {
            return;
        };
        if dx == 0 && dy == 0 {
            return;
        }
        unsafe { handle_mouse_delta(dx, dy) };
        return;
    }
    let (dx, dy) = (mouse.lLastX, mouse.lLastY);
    if dx == 0 && dy == 0 {
        return;
    }
    unsafe { handle_mouse_delta(dx, dy) };
}

/// Forward a relative mouse delta to the peer (capture active) or use it for
/// barrier detection (idle).
unsafe fn handle_mouse_delta(dx: i32, dy: i32) {
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

    // Buttons/wheel events injected by our own emulation while a peer drives
    // this machine (racing the local capture release) must not be reflected
    // back to the peer; pass them to the local applications instead. Only this
    // narrow race is guarded: injected events while no peer drives us keep
    // being forwarded (on-screen keyboards, macro tools, integration tests).
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    if REMOTE_PEER.get().is_some()
        && mouse_low_level.flags & LLMHF_INJECTED != 0
        && input_event::injection::is_recent(REMOTE_GRACE_MS)
    {
        return unsafe { CallNextHookEx(None, ncode, wparam, lparam) };
    }

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
    // Required by the hook contract: negative codes must be passed on.
    if ncode < 0 {
        return unsafe { CallNextHookEx(None, ncode, wparam, lparam) };
    }

    /* get active client if any */
    let Some(client) = ACTIVE_CLIENT.get() else {
        return unsafe { CallNextHookEx(None, ncode, wparam, lparam) };
    };

    // Key events injected by our own emulation (the peer drives this machine)
    // must neither be forwarded back to the peer (feedback loop) nor blocked
    // locally (the peer's input would be swallowed): pass them through. The
    // marker distinguishes our injection from third-party tools (on-screen
    // keyboard, macros), whose events keep being forwarded as before.
    // Only guard the case where a peer is actually driving this machine; the
    // existing behavior of forwarding injected keys while capturing must be
    // kept (the Windows integration test relies on it, and third-party
    // injectors such as on-screen keyboards are a valid input source).
    let keyboard_low_level: KBDLLHOOKSTRUCT = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
    if REMOTE_PEER.get().is_some()
        && keyboard_low_level.flags.contains(LLKHF_INJECTED)
        && input_event::injection::is_recent(REMOTE_GRACE_MS)
    {
        return unsafe { CallNextHookEx(None, ncode, wparam, lparam) };
    }

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
            log::debug!("display configuration changed");
            DISPLAY_RESOLUTION_GENERATION.fetch_add(1, Ordering::Release);
            if ACTIVE_CLIENT.get().is_some() {
                sync_active_capture_to_displays();
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // A display scale change (or the pointer ending up on a
            // differently scaled monitor) invalidates both the pinned entry
            // point and the physical->logical pixel ratio.
            log::debug!("display dpi changed");
            DISPLAY_RESOLUTION_GENERATION.fetch_add(1, Ordering::Release);
            if ACTIVE_CLIENT.get().is_some() {
                sync_active_capture_to_displays();
            }
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

/// Re-read the display layout and, when a capture is active, move the pinned
/// entry point (and the cursor overlay) into the new layout and refresh the
/// physical->logical pixel ratio. Called on WM_DISPLAYCHANGE/WM_DPICHANGED.
fn sync_active_capture_to_displays() {
    let mut cursor = POINT::default();
    let cursor_known = unsafe { GetCursorPos(&mut cursor) }.is_ok();
    let (entry_x, entry_y) = DISPLAYS.with_borrow_mut(|(displays, generation, primary)| {
        enumerate_displays(displays, primary);
        *generation = DISPLAY_RESOLUTION_GENERATION.load(Ordering::Acquire);
        let (ex, ey) = ENTRY_POINT.get();
        let prev = if cursor_known {
            (cursor.x, cursor.y)
        } else {
            (ex, ey)
        };
        display_util::clamp_to_display_bounds(displays, prev, (ex, ey))
    });
    ENTRY_POINT.replace((entry_x, entry_y));
    DPI_SCALE.replace(unsafe { monitor_scale((entry_x, entry_y)) });
    // Keep the overlay under the pinned pointer and re-pin the cursor to the
    // (possibly clamped) entry point.
    show_hidden_pointer((entry_x, entry_y));
    unsafe {
        let _ = SetCursorPos(entry_x, entry_y);
    }
    log::info!("capture pinned point refreshed after display change: ({entry_x}, {entry_y})");
}

static DISPLAY_RESOLUTION_GENERATION: AtomicI32 = AtomicI32::new(1);

fn update_display_regions(
    displays: &mut Vec<RECT>,
    generation: &mut i32,
    primary: &mut Option<RECT>,
) {
    let global_generation = DISPLAY_RESOLUTION_GENERATION.load(Ordering::Acquire);
    if *generation != global_generation {
        enumerate_displays(displays, primary);
        log::debug!("displays: {displays:?}");
        *generation = global_generation;
    }
}

/// Collect the monitor rectangles in virtual-desktop (physical pixel)
/// coordinates.
///
/// Uses `EnumDisplayMonitors`/`GetMonitorInfoW`, the same source the emulation
/// backend queries, so the capture barriers and the injected absolute cursor
/// position agree even on mixed-DPI layouts. `EnumDisplaySettingsW` was used
/// before, which reports per-display modes from a different code path.
fn enumerate_displays(display_rects: &mut Vec<RECT>, primary: &mut Option<RECT>) {
    unsafe extern "system" fn collect_monitor_with_primary(
        monitor: HMONITOR,
        hdc: HDC,
        rect: *mut RECT,
        user_data: LPARAM,
    ) -> BOOL {
        // SAFETY: same contract as `collect_monitor`; additionally read the
        // primary flag from GetMonitorInfoW and report it through the tuple.
        let state = unsafe { &mut *(user_data.0 as *mut (Vec<RECT>, Option<RECT>)) };
        collect_monitor_inner(monitor, hdc, rect, &mut state.0, &mut state.1);
        BOOL(1)
    }

    unsafe fn collect_monitor_inner(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        rects: &mut Vec<RECT>,
        primary: &mut Option<RECT>,
    ) {
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            if info.dwFlags & MONITORINFOF_PRIMARY != 0 {
                *primary = Some(info.rcMonitor);
            }
            rects.push(info.rcMonitor);
        } else {
            log::warn!("could not query the info of an attached monitor");
        }
    }

    let mut state = (std::mem::take(display_rects), None);
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor_with_primary),
            LPARAM(&mut state as *mut (Vec<RECT>, Option<RECT>) as isize),
        );
    }
    *display_rects = state.0;
    *primary = state.1;
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
        ClientUpdate::RemotePeer(peer) => {
            REMOTE_PEER.set(peer);
            match peer {
                Some(pos) => {
                    // A peer taking over must never leave a local capture (and
                    // a hidden cursor) behind, and the post-release cooldown
                    // must not swallow the return crossing through `pos`.
                    if ACTIVE_CLIENT.get().is_some() {
                        release_active_client();
                    }
                    LAST_RELEASE.replace(None);
                    log::debug!("remote peer control active from {pos:?}");
                }
                None => log::debug!("remote peer control inactive"),
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Position;

    #[test]
    fn the_remote_entry_edge_is_the_only_allowed_crossing() {
        assert!(edge_activation_allowed(None, Position::Left));
        assert!(edge_activation_allowed(None, Position::Bottom));
        assert!(edge_activation_allowed(
            Some(Position::Left),
            Position::Left
        ));
        assert!(!edge_activation_allowed(
            Some(Position::Left),
            Position::Right
        ));
        assert!(!edge_activation_allowed(
            Some(Position::Top),
            Position::Bottom
        ));
    }
}
