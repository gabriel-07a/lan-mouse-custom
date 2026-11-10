use async_trait::async_trait;
use futures_core::Stream;
use std::{
    collections::{HashSet, VecDeque},
    env,
    fmt::{self, Display},
    io::{self, ErrorKind},
    os::fd::{AsFd, RawFd},
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::io::unix::AsyncFd;

use std::{
    fs::File,
    io::{BufWriter, Write},
    os::unix::prelude::AsRawFd,
    sync::Arc,
};

use wayland_protocols::{
    wp::{
        keyboard_shortcuts_inhibit::zv1::client::{
            zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
            zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
        },
        pointer_constraints::zv1::client::{
            zwp_locked_pointer_v1::ZwpLockedPointerV1,
            zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1},
        },
        relative_pointer::zv1::client::{
            zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
            zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
        },
    },
    xdg::xdg_output::zv1::client::{
        zxdg_output_manager_v1::ZxdgOutputManagerV1,
        zxdg_output_v1::{self, ZxdgOutputV1},
    },
};

use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use wayland_client::{
    Connection, Dispatch, DispatchError, EventQueue, QueueHandle, WEnum,
    backend::{ReadEventsGuard, WaylandError},
    delegate_noop,
    globals::{Global, GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_compositor,
        wl_keyboard::{self, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{self, WlPointer},
        wl_region,
        wl_registry::{self, WlRegistry},
        wl_seat, wl_shm, wl_shm_pool,
        wl_surface::WlSurface,
    },
};

use input_event::{Event, KeyboardEvent, PointerEvent};

use crate::{CaptureError, CaptureEvent};

use super::{
    Capture, Position,
    error::{LayerShellCaptureCreationError, WaylandBindError},
};

struct Globals {
    compositor: wl_compositor::WlCompositor,
    pointer_constraints: ZwpPointerConstraintsV1,
    relative_pointer_manager: ZwpRelativePointerManagerV1,
    shortcut_inhibit_manager: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    seat: wl_seat::WlSeat,
    shm: wl_shm::WlShm,
    layer_shell: ZwlrLayerShellV1,
    xdg_output_manager: ZxdgOutputManagerV1,
}

#[derive(Clone, Debug)]
struct Output {
    wl_output: WlOutput,
    global: Global,
    info: Option<OutputInfo>,
    pending_info: OutputInfo,
    has_xdg_info: bool,
}

impl Display for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(info) = &self.info {
            write!(
                f,
                "{} {}x{} @pos {:?} ({})",
                info.name, info.size.0, info.size.1, info.position, info.description
            )
        } else {
            write!(f, "unknown output")
        }
    }
}

#[derive(Clone, Debug, Default)]
struct OutputInfo {
    description: String,
    name: String,
    position: (i32, i32),
    size: (i32, i32),
}

struct State {
    active_positions: HashSet<Position>,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    pointer_lock: Option<ZwpLockedPointerV1>,
    rel_pointer: Option<ZwpRelativePointerV1>,
    shortcut_inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
    active_windows: Vec<Arc<Window>>,
    focused: Option<Arc<Window>>,
    global_list: GlobalList,
    globals: Globals,
    read_guard: Option<ReadEventsGuard>,
    qh: QueueHandle<Self>,
    pending_events: VecDeque<(Position, CaptureEvent)>,
    outputs: Vec<Output>,
    scroll_discrete_pending: bool,
    /// the wayland connection is unusable; nothing more can be captured
    fatal: bool,
}

struct Inner {
    state: State,
    queue: EventQueue<State>,
}

impl AsRawFd for Inner {
    fn as_raw_fd(&self) -> RawFd {
        self.queue.as_fd().as_raw_fd()
    }
}

pub struct LayerShellInputCapture(AsyncFd<Inner>);

struct Window {
    buffer: wl_buffer::WlBuffer,
    surface: WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    pos: Position,
    /// Logical position of the output this window is anchored to; used to map
    /// the pointer's surface coordinates to a layout edge fraction.
    output_pos: (i32, i32),
}

impl Window {
    fn new(
        state: &State,
        qh: &QueueHandle<State>,
        output: &WlOutput,
        output_pos: (i32, i32),
        pos: Position,
        size: (i32, i32),
    ) -> io::Result<Window> {
        log::debug!("creating window output: {output:?}, size: {size:?}");
        let g = &state.globals;

        // A compositor that reports a non-positive or absurdly large logical
        // output size must not make the integer arithmetic below overflow.
        if size.0 <= 0 || size.1 <= 0 {
            return Err(io::Error::other(format!(
                "invalid output size {size:?} for position {pos}"
            )));
        }
        let (width, height) = match pos {
            Position::Left | Position::Right => (1, size.1 as u32),
            Position::Top | Position::Bottom => (size.0 as u32, 1),
        };
        let len = u64::from(width) * u64::from(height) * 4;
        let len = i32::try_from(len).map_err(|_| {
            io::Error::other(format!("output size {size:?} exceeds the shm buffer limit"))
        })?;
        let mut file = tempfile::tempfile()?;
        draw(&mut file, (width, height))?;
        let pool = g.shm.create_pool(file.as_fd(), len, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            (width * 4) as i32,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        // The pool can be destroyed right after the buffer was created: the
        // buffer keeps the underlying memory alive. Not destroying it leaked a
        // server-side object (and the fd mapping) for every window, e.g. on
        // every display reconfigure.
        pool.destroy();
        let surface = g.compositor.create_surface(qh, ());

        let layer_surface = g.layer_shell.get_layer_surface(
            &surface,
            Some(output),
            Layer::Overlay,
            "LAN Mouse Sharing".into(),
            qh,
            (),
        );
        let anchor = match pos {
            Position::Left => Anchor::Left,
            Position::Right => Anchor::Right,
            Position::Top => Anchor::Top,
            Position::Bottom => Anchor::Bottom,
        };

        layer_surface.set_anchor(anchor);
        layer_surface.set_size(width, height);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_margin(0, 0, 0, 0);
        surface.set_input_region(None);
        surface.commit();
        Ok(Window {
            pos,
            output_pos,
            buffer,
            surface,
            layer_surface,
        })
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        log::debug!("destroying window!");
        self.layer_surface.destroy();
        self.surface.destroy();
        self.buffer.destroy();
    }
}

fn get_edges(outputs: &[Output], pos: Position) -> Vec<(Output, i32)> {
    outputs
        .iter()
        .filter_map(|output| {
            output.info.as_ref().map(|info| {
                (
                    output.clone(),
                    match pos {
                        Position::Left => info.position.0,
                        Position::Right => info.position.0 + info.size.0,
                        Position::Top => info.position.1,
                        Position::Bottom => info.position.1 + info.size.1,
                    },
                )
            })
        })
        .collect()
}

/// Normalised position (0.0..=1.0) of `coord` along the layout edge `pos`,
/// using the union of the outputs touching that edge as the span.
fn edge_fraction(outputs: &[Output], pos: Position, coord: i32) -> Option<f32> {
    let mut min = i32::MAX;
    let mut max = i32::MIN;
    for info in outputs.iter().filter_map(|output| output.info.as_ref()) {
        let (lo, hi) = match pos {
            Position::Left | Position::Right => (info.position.1, info.position.1 + info.size.1),
            Position::Top | Position::Bottom => (info.position.0, info.position.0 + info.size.0),
        };
        min = min.min(lo);
        max = max.max(hi);
    }
    let span = max.saturating_sub(min);
    if span <= 0 {
        return None;
    }
    Some(((coord - min) as f32 / span as f32).clamp(0.0, 1.0))
}

fn get_output_configuration(state: &State, pos: Position) -> Vec<Output> {
    // get all output edges corresponding to the position
    let edges = get_edges(&state.outputs, pos);
    let opposite_edges = get_edges(&state.outputs, pos.opposite());

    // remove those edges that are at the same position
    // as an opposite edge of a different output
    edges
        .iter()
        .filter(|(_, edge)| !opposite_edges.iter().map(|(_, e)| *e).any(|e| &e == edge))
        .map(|(o, _)| o.clone())
        .collect()
}

fn draw(f: &mut File, (width, height): (u32, u32)) -> io::Result<()> {
    if env::var_os("LM_DEBUG_LAYER_SHELL").is_some() {
        let mut buf = BufWriter::new(f);
        for _ in 0..width * height {
            // AARRGGBB
            buf.write_all(&0xff11d116u32.to_ne_bytes())?;
        }
        buf.flush()?;
    } else {
        // Transparent ARGB8888; an all-zero sparse file is the same content
        // without faulting in (and writing) every page.
        f.set_len(u64::from(width) * u64::from(height) * 4)?;
    }
    Ok(())
}

impl LayerShellInputCapture {
    pub fn new() -> std::result::Result<Self, LayerShellCaptureCreationError> {
        let conn = Connection::connect_to_env()?;
        let (global_list, mut queue) = registry_queue_init::<State>(&conn)?;

        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor = global_list
            .bind(&qh, 4..=5, ())
            .map_err(|e| WaylandBindError::new(e, "wl_compositor 4..=5"))?;
        let xdg_output_manager: ZxdgOutputManagerV1 = global_list
            .bind(&qh, 1..=3, ())
            .map_err(|e| WaylandBindError::new(e, "xdg_output_manager 1..=3"))?;
        let shm: wl_shm::WlShm = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "wl_shm"))?;
        let layer_shell: ZwlrLayerShellV1 = global_list
            .bind(&qh, 3..=4, ())
            .map_err(|e| WaylandBindError::new(e, "wlr_layer_shell 3..=4"))?;
        let seat: wl_seat::WlSeat = global_list
            .bind(&qh, 7..=8, ())
            .map_err(|e| WaylandBindError::new(e, "wl_seat 7..=8"))?;

        let pointer_constraints: ZwpPointerConstraintsV1 = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "zwp_pointer_constraints_v1"))?;
        let relative_pointer_manager: ZwpRelativePointerManagerV1 = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "zwp_relative_pointer_manager_v1"))?;
        let shortcut_inhibit_manager: Result<
            ZwpKeyboardShortcutsInhibitManagerV1,
            WaylandBindError,
        > = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "zwp_keyboard_shortcuts_inhibit_manager_v1"));
        // layer-shell backend still works without this protocol so we make it an optional dependency
        if let Err(e) = &shortcut_inhibit_manager {
            log::warn!("shortcut_inhibit_manager not supported: {e}\nkeybinds handled by the compositor will not be passed
                to the client");
        }
        let shortcut_inhibit_manager = shortcut_inhibit_manager.ok();

        let mut state = State {
            active_positions: Default::default(),
            pointer: None,
            keyboard: None,
            global_list,
            globals: Globals {
                compositor,
                shm,
                layer_shell,
                seat,
                pointer_constraints,
                relative_pointer_manager,
                shortcut_inhibit_manager,
                xdg_output_manager,
            },
            pointer_lock: None,
            rel_pointer: None,
            shortcut_inhibitor: None,
            active_windows: Vec::new(),
            focused: None,
            qh,
            read_guard: None,
            pending_events: VecDeque::new(),
            outputs: vec![],
            scroll_discrete_pending: false,
            fatal: false,
        };

        for global in state.global_list.contents().clone_list() {
            state.register_global(global);
        }

        // flush outgoing events
        queue.flush()?;

        let read_guard = loop {
            match queue.prepare_read() {
                Some(r) => break r,
                None => {
                    queue.dispatch_pending(&mut state)?;
                    continue;
                }
            }
        };
        state.read_guard = Some(read_guard);

        let inner = AsyncFd::new(Inner { queue, state })?;

        Ok(LayerShellInputCapture(inner))
    }

    fn add_client(&mut self, pos: Position) -> io::Result<()> {
        self.0.get_mut().state.add_client(pos)
    }

    fn delete_client(&mut self, pos: Position) {
        let inner = self.0.get_mut();
        inner.state.active_positions.remove(&pos);
        // Releasing the grab of the window that is about to be destroyed keeps
        // the pointer lock/interactivity from outliving their surface.
        if inner.state.focused.as_ref().is_some_and(|w| w.pos == pos) {
            inner.state.ungrab();
            inner.state.focused = None;
        }
        // remove all windows corresponding to this client
        while let Some(i) = inner.state.active_windows.iter().position(|w| w.pos == pos) {
            inner.state.active_windows.remove(i);
            if inner.state.focused.as_ref().is_some_and(|w| w.pos == pos) {
                inner.state.focused = None;
            }
        }
    }
}

impl State {
    fn update_output_info(&mut self, name: u32) {
        // A global can be removed (monitor unplugged) before its pending
        // wl_output/xdg_output events are dispatched; an event for an output we
        // no longer track is stale, not a reason to abort the process.
        let Some(output) = self.outputs.iter_mut().find(|o| o.global.name == name) else {
            log::debug!("ignoring output event for unknown global {name}");
            return;
        };
        if output.has_xdg_info {
            output.info.replace(output.pending_info.clone());
            self.update_windows();
        }
    }

    fn register_global(&mut self, global: Global) {
        if global.interface.as_str() == "wl_output" {
            log::debug!("new output global: wl_output {}", global.name);
            // Bind to what the compositor advertises (capped at the version
            // this client supports): binding a version above the advertised
            // one is a protocol error that would kill the connection.
            let version = global.version.min(4);
            let wl_output = self.global_list.registry().bind::<WlOutput, _, _>(
                global.name,
                version,
                &self.qh,
                global.name,
            );
            self.globals
                .xdg_output_manager
                .get_xdg_output(&wl_output, &self.qh, global.name);
            self.outputs.push(Output {
                wl_output,
                global,
                info: None,
                has_xdg_info: false,
                pending_info: Default::default(),
            })
        }
    }

    fn deregister_global(&mut self, name: u32) {
        self.outputs.retain(|o| {
            if o.global.name == name {
                log::debug!("{o} (global {:?}) removed", o.global);
                o.wl_output.release();
                false
            } else {
                true
            }
        });
    }

    fn grab(
        &mut self,
        surface: &WlSurface,
        pointer: &WlPointer,
        serial: u32,
        qh: &QueueHandle<State>,
    ) {
        let Some(window) = self.focused.as_ref() else {
            return;
        };

        // hide the cursor
        pointer.set_cursor(serial, None, 0, 0);

        // capture input
        window
            .layer_surface
            .set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        window.surface.commit();

        // lock pointer
        if self.pointer_lock.is_none() {
            self.pointer_lock = Some(self.globals.pointer_constraints.lock_pointer(
                surface,
                pointer,
                None,
                Lifetime::Persistent,
                qh,
                (),
            ));
        }

        // request relative input
        if self.rel_pointer.is_none() {
            self.rel_pointer = Some(self.globals.relative_pointer_manager.get_relative_pointer(
                pointer,
                qh,
                (),
            ));
        }

        // capture modifier keys
        if let Some(shortcut_inhibit_manager) = &self.globals.shortcut_inhibit_manager {
            if self.shortcut_inhibitor.is_none() {
                self.shortcut_inhibitor = Some(shortcut_inhibit_manager.inhibit_shortcuts(
                    surface,
                    &self.globals.seat,
                    qh,
                    (),
                ));
            }
        }
    }

    fn ungrab(&mut self) {
        // NOTE: `grab` hides the cursor with `wl_pointer.set_cursor(None)`.
        // Restoring the compositor's default cursor on release has no
        // dedicated Wayland request; whether the pointer reappears when the
        // lock/interactivity is dropped is compositor-dependent and still
        // needs validation on Sway/Hyprland. Do not add a speculative cursor
        // theme dependency here without that check.

        // ungrab surface if we still have one
        if let Some(window) = self.focused.as_ref() {
            window
                .layer_surface
                .set_keyboard_interactivity(KeyboardInteractivity::None);
            window.surface.commit();
        }

        // The grab objects must be destroyed even when there is no focused
        // window: they may have been created for a window that was destroyed
        // in the meantime (display reconfigure, capture teardown). Leaving
        // them behind would make every later `grab` skip creating them and
        // the capture would run without pointer lock / shortcut inhibition.
        if let Some(pointer_lock) = self.pointer_lock.take() {
            pointer_lock.destroy();
        }
        if let Some(rel_pointer) = self.rel_pointer.take() {
            rel_pointer.destroy();
        }
        if let Some(shortcut_inhibitor) = self.shortcut_inhibitor.take() {
            shortcut_inhibitor.destroy();
        }
        self.scroll_discrete_pending = false;
    }

    fn add_client(&mut self, pos: Position) -> io::Result<()> {
        self.active_positions.insert(pos);
        let outputs = get_output_configuration(self, pos);

        log::info!(
            "adding capture for position {pos} - using outputs: {:?}",
            outputs
                .iter()
                .map(|o| o
                    .info
                    .as_ref()
                    .map(|i| i.name.to_owned())
                    .unwrap_or("unknown output".to_owned()))
                .collect::<Vec<_>>()
        );
        let mut first_error = None;
        for output in outputs.iter() {
            let Some(info) = output.info.as_ref() else {
                continue;
            };
            match Window::new(
                self,
                &self.qh,
                &output.wl_output,
                info.position,
                pos,
                info.size,
            ) {
                Ok(window) => self.active_windows.push(Arc::new(window)),
                Err(e) => {
                    log::warn!("could not create the capture window on {output}: {e}");
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
            }
        }
        // A position without a single window can never report input; report
        // the failure instead of silently accepting the capture. The position
        // must not stay registered: a later output hotplug would recreate
        // windows for it and they could grab the pointer while no client
        // exists to release it (InputCapture rolled its own bookkeeping back).
        if self.active_windows.iter().all(|window| window.pos != pos) {
            self.active_positions.remove(&pos);
            return Err(first_error.unwrap_or_else(|| {
                io::Error::other(format!("no capture window available for position {pos}"))
            }));
        }
        Ok(())
    }

    fn update_windows(&mut self) {
        log::info!("active outputs: ");
        for output in self.outputs.iter().filter(|o| o.info.is_some()) {
            log::info!(" * {output}");
        }

        // Outputs were reconfigured (hotplug, resolution, arrangement change).
        // The windows are about to be destroyed; release the grab first so the
        // pointer lock / keyboard interactivity are not left attached to
        // destroyed surfaces (which would also keep the next grab from being
        // created).
        self.ungrab();
        self.active_windows.clear();
        self.focused = None;

        let active_positions = self.active_positions.iter().cloned().collect::<Vec<_>>();
        for pos in active_positions {
            if let Err(e) = self.add_client(pos) {
                log::warn!("could not recreate capture for position {pos}: {e}");
            }
        }
    }
}

impl Inner {
    /// Read from the wayland socket.
    ///
    /// A non-`WouldBlock` IO error means the connection is gone (compositor
    /// crashed or restarted). It used to be only logged, which made
    /// [`LayerShellInputCapture::poll_next`] spin on the permanently readable
    /// (EOF) socket and wedge the whole single-threaded runtime at 100% CPU.
    fn read(&mut self) -> Result<(), CaptureError> {
        let Some(guard) = self.state.read_guard.take() else {
            return Err(CaptureError::Io(io::Error::other(
                "wayland read guard missing",
            )));
        };
        match guard.read() {
            Ok(_) => Ok(()),
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => Ok(()),
            Err(WaylandError::Io(e)) => Err(CaptureError::Io(e)),
            Err(WaylandError::Protocol(e)) => Err(wayland_protocol_error(e)),
        }
    }

    fn prepare_read(&mut self) -> Result<(), CaptureError> {
        // `prepare_read` returns None while outgoing requests still have to be
        // flushed. Dispatch what is buffered and retry a bounded number of
        // times: if the send buffer is full the loop must return to the
        // executor instead of spinning at 100% CPU.
        for _ in 0..16 {
            if let Some(guard) = self.queue.prepare_read() {
                self.state.read_guard = Some(guard);
                return Ok(());
            }
            self.dispatch_events()?;
        }
        Ok(())
    }

    fn dispatch_events(&mut self) -> Result<(), CaptureError> {
        match self.queue.dispatch_pending(&mut self.state) {
            Ok(_) => Ok(()),
            Err(DispatchError::Backend(WaylandError::Io(e))) => Err(CaptureError::Io(e)),
            Err(DispatchError::Backend(WaylandError::Protocol(e))) => {
                Err(wayland_protocol_error(e))
            }
            Err(DispatchError::BadMessage {
                sender_id,
                interface,
                opcode,
            }) => Err(CaptureError::Io(io::Error::other(format!(
                "wayland bad message {sender_id}, {interface}, {opcode}"
            )))),
        }
    }

    fn flush_events(&mut self) -> Result<bool, CaptureError> {
        // Flush outgoing events. A full socket send buffer is transient (the
        // compositor is busy or suspended): it must not be treated as a dead
        // connection, or one slow moment would permanently disable capture.
        // The caller retries on the next poll.
        match self.queue.flush() {
            Ok(()) => Ok(false),
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => Ok(true),
            Err(WaylandError::Io(e)) => Err(CaptureError::Io(e)),
            Err(WaylandError::Protocol(e)) => Err(wayland_protocol_error(e)),
        }
    }
}

/// A wayland protocol violation is fatal for this connection; surface it as a
/// capture error so the service can recreate the backend instead of aborting
/// the whole process from inside a dispatch callback.
fn wayland_protocol_error(e: wayland_client::backend::protocol::ProtocolError) -> CaptureError {
    CaptureError::Io(io::Error::other(format!("wayland protocol error: {e}")))
}

#[async_trait]
impl Capture for LayerShellInputCapture {
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.add_client(pos)?;
        let inner = self.0.get_mut();
        // A temporarily full send buffer is not an error: the request reaches
        // the compositor on a later flush.
        inner.flush_events().map(|_| ())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.delete_client(pos);
        let inner = self.0.get_mut();
        inner.flush_events().map(|_| ())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        log::debug!("releasing pointer");
        let inner = self.0.get_mut();
        inner.state.ungrab();
        inner.flush_events().map(|_| ())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        // Make sure a grab that is still active does not outlive the backend.
        let inner = self.0.get_mut();
        inner.state.ungrab();
        let _ = inner.flush_events();
        Ok(())
    }
}

impl Stream for LayerShellInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // The wayland connection died (compositor restart, socket EOF): the
        // stream is over. Returning `Pending` here would spin forever because
        // EOF keeps the fd readable.
        if self.0.get_mut().state.fatal {
            return Poll::Ready(None);
        }

        if let Some(event) = self.0.get_mut().state.pending_events.pop_front() {
            return Poll::Ready(Some(Ok(event)));
        }

        loop {
            let mut guard = ready!(self.0.poll_read_ready_mut(cx))?;

            let result = {
                let inner = guard.get_inner_mut();
                inner
                    .read()
                    .and_then(|()| inner.dispatch_events())
                    .and_then(|()| {
                        // A transient WouldBlock means the compositor is not
                        // draining our socket right now; return to the executor and
                        // retry on the next readiness event instead of declaring
                        // the whole connection dead.
                        match inner.flush_events() {
                            Ok(true) => Ok(false),
                            Ok(false) => inner.prepare_read().map(|()| true),
                            Err(e) => Err(e),
                        }
                    })
            };

            // clear read readiness for tokio read guard
            guard.clear_ready();

            match result {
                Ok(true) => {}
                Ok(false) => return Poll::Pending,
                Err(e) => {
                    let inner = guard.get_inner_mut();
                    inner.state.fatal = true;
                    log::error!("layer-shell input capture connection lost: {e}");
                    return Poll::Ready(Some(Err(e)));
                }
            }

            // if an event has been queued during dispatch_events() we return it
            match guard.get_inner_mut().state.pending_events.pop_front() {
                Some(event) => return Poll::Ready(Some(Ok(event))),
                None => continue,
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: <wl_seat::WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Pointer) {
                if let Some(p) = state.pointer.take() {
                    p.release();
                }
                state.pointer.replace(seat.get_pointer(qh, ()));
            }
            if capabilities.contains(wl_seat::Capability::Keyboard) {
                // Same lifetime rule as the pointer: release the previous
                // wl_keyboard before creating a new one. The old code dropped
                // the proxy without releasing it (leak) and left
                // `state.keyboard` dead, so capability churn accumulated
                // wl_keyboard objects and could forward duplicated key events.
                if let Some(k) = state.keyboard.take() {
                    k.release();
                }
                state.keyboard.replace(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        app: &mut Self,
        pointer: &WlPointer,
        event: <WlPointer as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                // get client corresponding to the focused surface
                let Some(window) = app
                    .active_windows
                    .iter()
                    .find(|w| w.surface == surface)
                    .cloned()
                else {
                    return;
                };
                let pos = window.pos;
                // The 1-pixel strip covers the full output edge, so the
                // surface coordinate plus the output position gives the
                // crossing point in layout coordinates.
                let along = match pos {
                    Position::Left | Position::Right => {
                        edge_fraction(&app.outputs, pos, window.output_pos.1 + surface_y as i32)
                    }
                    Position::Top | Position::Bottom => {
                        edge_fraction(&app.outputs, pos, window.output_pos.0 + surface_x as i32)
                    }
                };
                let begin = match along {
                    Some(along) => CaptureEvent::BeginAt(along),
                    None => CaptureEvent::Begin,
                };
                app.focused = Some(window);
                app.grab(&surface, pointer, serial, qh);
                app.pending_events.push_back((pos, begin));
            }
            wl_pointer::Event::Leave { .. } => {
                /* There are rare cases, where when a window is opened in
                 * just the wrong moment, the pointer is released, while
                 * still grabbed.
                 * In that case, the pointer must be ungrabbed, otherwise
                 * it is impossible to grab it again (since the pointer
                 * lock, relative pointer,... objects are still in place)
                 */
                if app.pointer_lock.is_some() {
                    log::warn!("compositor released mouse");
                }
                app.ungrab();
                // The capture is over for this surface. Keeping `focused`
                // would attribute keyboard events that are still in flight to
                // the old client after the release.
                app.focused = None;
            }
            wl_pointer::Event::Button {
                serial: _,
                time,
                button,
                state,
            } => {
                let Some(window) = app.focused.as_ref() else {
                    return;
                };
                app.pending_events.push_back((
                    window.pos,
                    CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                        time,
                        button,
                        state: u32::from(state),
                    })),
                ));
            }
            wl_pointer::Event::Axis { time, axis, value } => {
                let Some(window) = app.focused.as_ref() else {
                    app.scroll_discrete_pending = false;
                    return;
                };
                if app.scroll_discrete_pending {
                    // each axisvalue120 event is coupled with
                    // a corresponding axis event, which needs to
                    // be ignored to not duplicate the scrolling
                    app.scroll_discrete_pending = false;
                } else {
                    app.pending_events.push_back((
                        window.pos,
                        CaptureEvent::Input(Event::Pointer(PointerEvent::Axis {
                            time,
                            axis: u32::from(axis) as u8,
                            value,
                        })),
                    ));
                }
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                let Some(window) = app.focused.as_ref() else {
                    return;
                };
                app.scroll_discrete_pending = true;
                app.pending_events.push_back((
                    window.pos,
                    CaptureEvent::Input(Event::Pointer(PointerEvent::AxisDiscrete120 {
                        axis: u32::from(axis) as u8,
                        value: value120,
                    })),
                ));
            }
            wl_pointer::Event::Frame => {
                // TODO properly handle frame events
                // we simply insert a frame event on the client side
                // after each event for now
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        app: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let window = &app.focused;
        match event {
            wl_keyboard::Event::Key {
                serial: _,
                time,
                key,
                state,
            } => {
                if let Some(window) = window {
                    app.pending_events.push_back((
                        window.pos,
                        CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                            time,
                            key,
                            state: u32::from(state) as u8,
                        })),
                    ));
                }
            }
            wl_keyboard::Event::Modifiers {
                serial: _,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                if let Some(window) = window {
                    app.pending_events.push_back((
                        window.pos,
                        CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                            depressed: mods_depressed,
                            latched: mods_latched,
                            locked: mods_locked,
                            group,
                        })),
                    ));
                }
            }
            _ => (),
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for State {
    fn event(
        app: &mut Self,
        _: &ZwpRelativePointerV1,
        event: <ZwpRelativePointerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            utime_hi,
            utime_lo,
            dx_unaccel: dx,
            dy_unaccel: dy,
            ..
        } = event
        {
            if let Some(window) = &app.focused {
                let time = ((((utime_hi as u64) << 32) | utime_lo as u64) / 1000) as u32;
                app.pending_events.push_back((
                    window.pos,
                    CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { time, dx, dy })),
                ));
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        app: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            if let Some(window) = app
                .active_windows
                .iter()
                .find(|w| &w.layer_surface == layer_surface)
            {
                // client corresponding to the layer_surface
                let surface = &window.surface;
                let buffer = &window.buffer;
                surface.attach(Some(buffer), 0, 0);
                layer_surface.ack_configure(serial);
                surface.commit();
            }
        }
    }
}

// delegate wl_registry events to App itself
impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        _registry: &WlRegistry,
        event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                state.register_global(Global {
                    name,
                    interface,
                    version,
                });
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.deregister_global(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: <ZxdgOutputV1 as wayland_client::Proxy>::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // A removed global (monitor unplugged) can still have events queued.
        let Some(output) = state.outputs.iter_mut().find(|o| o.global.name == *name) else {
            log::debug!("ignoring xdg_output event for unknown global {name}");
            return;
        };

        log::debug!("xdg_output {name} - {event:?}");
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                output.pending_info.position = (x, y);
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output.pending_info.size = (width, height);
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::Done => {
                log::warn!("Use of deprecated xdg-output event \"done\"");
                state.update_output_info(*name);
            }
            zxdg_output_v1::Event::Name { name } => {
                output.pending_info.name = name;
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::Description { description } => {
                output.pending_info.description = description;
                output.has_xdg_info = true;
            }
            // A newer version of the protocol may add events; ignore them
            // instead of panicking on a compositor that speaks them.
            other => log::trace!("unhandled xdg_output event: {other:?}"),
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _wl_output: &WlOutput,
        event: <WlOutput as wayland_client::Proxy>::Event,
        name: &u32,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        log::debug!("wl_output {name} - {event:?}");
        if let wl_output::Event::Done = event {
            state.update_output_info(*name);
        }
    }
}

// don't emit any events
delegate_noop!(State: wl_region::WlRegion);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: wl_compositor::WlCompositor);
delegate_noop!(State: ZwlrLayerShellV1);
delegate_noop!(State: ZwpRelativePointerManagerV1);
delegate_noop!(State: ZwpKeyboardShortcutsInhibitManagerV1);
delegate_noop!(State: ZwpPointerConstraintsV1);

// ignore events
delegate_noop!(State: ignore ZxdgOutputManagerV1);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore ZwpKeyboardShortcutsInhibitorV1);
delegate_noop!(State: ignore ZwpLockedPointerV1);
