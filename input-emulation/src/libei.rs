use futures::{StreamExt, future};
use std::{
    collections::HashSet,
    io,
    os::{fd::OwnedFd, unix::net::UnixStream},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;

use ashpd::desktop::{
    PersistMode, Session,
    remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions},
};
use async_trait::async_trait;

use reis::{
    ei::{
        self, Button, Keyboard, Pointer, PointerAbsolute, Scroll, button::ButtonState,
        handshake::ContextType, keyboard::KeyState,
    },
    event::{self, Connection, DeviceCapability, DeviceEvent, EiEvent, Region, SeatEvent},
    tokio::EiConvertEventStream,
};

use input_event::{Event, KeyboardEvent, PointerEvent};

use crate::{
    error::EmulationError,
    portal_token::{REMOTE_DESKTOP_TOKEN_FILE, read_named_token, write_named_token},
};

use super::{
    EdgeReturnNotifier, Emulation, EmulationHandle, PointerEdge, error::LibeiEmulationCreationError,
};

/// Absolute pointer device advertised by the compositor, with the EIS region
/// that defines our coordinate space.
#[derive(Clone)]
struct AbsolutePointer {
    device: ei::Device,
    pointer: PointerAbsolute,
    /// changes whenever the compositor adds/replaces the absolute device
    generation: u64,
    /// device regions in logical pixels (x0, y0, x1, y1)
    regions: Vec<(f32, f32, f32, f32)>,
    /// logical-to-physical scale of each region (same order as regions)
    scales: Vec<f32>,
}

#[derive(Clone, Default)]
struct Devices {
    pointer: Arc<RwLock<Option<(ei::Device, ei::Pointer)>>>,
    /// set when the compositor exposes an absolute pointer device (libei
    /// PI_ABSOLUTE): lets us position the pointer exactly and detect the
    /// return hand-off at the entry edge
    absolute: Arc<RwLock<Option<AbsolutePointer>>>,
    absolute_generation: Arc<AtomicU64>,
    /// monotonically increasing sequence for ei_device.start_emulating
    resume_sequence: Arc<AtomicU64>,
    /// EIS devices the compositor paused; absolute events must not be sent then
    paused: Arc<RwLock<HashSet<ei::Device>>>,
    scroll: Arc<RwLock<Option<(ei::Device, ei::Scroll)>>>,
    button: Arc<RwLock<Option<(ei::Device, ei::Button)>>>,
    keyboard: Arc<RwLock<Option<(ei::Device, ei::Keyboard)>>>,
}

impl Devices {
    /// Forget every interface belonging to a device the compositor removed.
    ///
    /// The EIS objects are destroyed with the device; sending events on a
    /// destroyed object id is a protocol violation and can tear down the whole
    /// EIS connection (all input emulation then fails). Fresh interfaces arrive
    /// with the next `DeviceAdded`.
    fn forget_device(&self, device: &ei::Device) {
        fn clear<T>(slot: &RwLock<Option<(ei::Device, T)>>, device: &ei::Device) {
            let mut slot = slot.write().unwrap_or_else(|e| e.into_inner());
            // The high-level Device wrapper is keyed by the ei_device object,
            // so equality is object identity.
            if slot.as_ref().is_some_and(|(stored, _)| stored == device) {
                *slot = None;
            }
        }
        clear(&self.pointer, device);
        clear(&self.scroll, device);
        clear(&self.button, device);
        clear(&self.keyboard, device);
        self.paused
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(device);
        {
            let mut absolute = self
                .absolute
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if absolute
                .as_ref()
                .is_some_and(|stored| &stored.device == device)
            {
                *absolute = None;
            }
        }
    }
}

pub(crate) struct LibeiEmulation {
    context: ei::Context,
    conn: event::Connection,
    devices: Devices,
    ei_task: JoinHandle<()>,
    error: Arc<Mutex<Option<EmulationError>>>,
    libei_error: Arc<AtomicBool>,
    _remote_desktop: RemoteDesktop,
    session: Session<RemoteDesktop>,
    /// edge the peer entered through, while it drives this machine
    remote_edge: Option<PointerEdge>,
    /// exact absolute-position tracker once armed
    absolute: Option<AbsoluteTracker>,
    /// service notifier for the return hand-off
    edge_return: Option<EdgeReturnNotifier>,
    /// warning about a missing absolute pointer is logged once per activation
    absolute_warned: bool,
    /// monotonic clock (EI frame timestamps must use CLOCK_MONOTONIC)
    clock: Instant,
}

/// Pixels the pointer must be pushed past the entry edge before the hand-off
/// fires. The absolute tracker knows the exact position, so this can be tiny.
const RETURN_MARGIN_PX: f64 = 3.0;

/// Distance (logical px) the cursor starts inside the entry edge. Big enough
/// that hand-off jitter cannot fire immediately, small enough that the cursor
/// visibly appears on the side the peer came from.
const EDGE_ANCHOR_INSET: f64 = 24.0;

/// Largest per-event delta accepted from the wire. Bigger values are treated
/// as corrupted packets: one bogus delta must not jump the cursor across the
/// screen and trigger a false hand-off.
const MAX_DELTA_PX: f64 = 4096.0;

/// Exact edge-return detector for absolute pointer injection.
///
/// The pointer is warped to a known anchor on entry and every injected motion
/// delta updates the tracked position. Because the same position is sent to
/// the compositor as an absolute motion event, the tracked position *is* the
/// real cursor position: crossing the entry edge is unambiguous.
///
/// The tracked position lives in the EIS layout coordinate space (all monitor
/// regions share it), so the pointer can roam across monitors; only the entry
/// edge of the region that was entered triggers the hand-off.
#[derive(Clone, Debug)]
struct AbsoluteTracker {
    edge: PointerEdge,
    /// absolute device generation this tracker was armed against
    generation: u64,
    /// multiplies incoming deltas: 1/monitor-scale so the physical speed
    /// matches the previous relative injection (and a user override)
    delta_scale: f64,
    /// monitor regions in logical pixels: (x0, y0, x1, y1), half-open
    regions: Vec<(f32, f32, f32, f32)>,
    /// region touching the entry edge (anchor + return threshold)
    entry: (f32, f32, f32, f32),
    layout_min_x: f32,
    layout_min_y: f32,
    layout_max_x: f32,
    layout_max_y: f32,
    x: f64,
    y: f64,
    fired: bool,
}

impl AbsoluteTracker {
    fn new(
        edge: PointerEdge,
        generation: u64,
        regions: Vec<(f32, f32, f32, f32)>,
        delta_scale: f64,
    ) -> Option<Self> {
        let entry = entry_region_bounds(&regions, edge)?;
        let layout_min_x = regions.iter().map(|r| r.0).fold(f32::INFINITY, f32::min);
        let layout_min_y = regions.iter().map(|r| r.1).fold(f32::INFINITY, f32::min);
        let layout_max_x = regions
            .iter()
            .map(|r| r.2)
            .fold(f32::NEG_INFINITY, f32::max);
        let layout_max_y = regions
            .iter()
            .map(|r| r.3)
            .fold(f32::NEG_INFINITY, f32::max);
        Some(Self {
            edge,
            generation,
            delta_scale,
            regions,
            entry,
            layout_min_x,
            layout_min_y,
            layout_max_x,
            layout_max_y,
            x: match edge {
                PointerEdge::Left => f64::from(entry.0) + EDGE_ANCHOR_INSET,
                PointerEdge::Right => f64::from(entry.2) - 1.0 - EDGE_ANCHOR_INSET,
                _ => f64::from(entry.0 + (entry.2 - entry.0) / 2.0),
            },
            y: match edge {
                PointerEdge::Top => f64::from(entry.1) + EDGE_ANCHOR_INSET,
                PointerEdge::Bottom => f64::from(entry.3) - 1.0 - EDGE_ANCHOR_INSET,
                _ => f64::from(entry.1 + (entry.3 - entry.1) / 2.0),
            },
            fired: false,
        })
    }

    /// The entry region bounds (return threshold).
    fn entry_bounds(&self) -> (f32, f32, f32, f32) {
        self.entry
    }

    /// Clamp a real cursor position into the layout (half-open region bounds).
    fn clamped(&self, x: f64, y: f64) -> (f64, f64) {
        let max_x = f64::from(self.layout_max_x) - 1.0;
        let max_y = f64::from(self.layout_max_y) - 1.0;
        (
            x.clamp(f64::from(self.layout_min_x), max_x),
            y.clamp(f64::from(self.layout_min_y), max_y),
        )
    }

    /// Apply one injected delta and report the hand-off edge once.
    fn apply(&mut self, dx: f64, dy: f64) -> Option<PointerEdge> {
        if !dx.is_finite() || !dy.is_finite() {
            return None;
        }
        // Corrupted single deltas would jump the tracked cursor anywhere; drop
        // them instead of clamping (a clamp could still fire the hand-off).
        if dx.abs() > MAX_DELTA_PX || dy.abs() > MAX_DELTA_PX {
            log::debug!("dropping implausible absolute delta ({dx}, {dy})");
            return None;
        }
        // HiDPI: the EIS region is in logical pixels while the peer's deltas
        // are physical-ish; dividing by the monitor scale keeps the physical
        // cursor speed identical to the previous relative injection.
        let (dx, dy) = (dx * self.delta_scale, dy * self.delta_scale);
        let raw_x = self.x + dx;
        let raw_y = self.y + dy;
        // The compositor clamps the real cursor into the layout; mirror that so
        // overshooting an edge and coming back behaves correctly.
        let (clamped_x, clamped_y) = self.clamped(raw_x, raw_y);
        // The position that will actually be sent: snapped into a real region.
        // Keeping the tracked position equal to the sent one prevents a desync
        // when the cursor is pushed into a gap between monitors.
        let (x, y) = self.snap(clamped_x, clamped_y);

        if self.fired {
            self.x = x;
            self.y = y;
            // Re-arm when the pointer is well inside the entry screen area.
            let inset = 20.0;
            if self.x > f64::from(self.entry.0) + inset
                && self.x < f64::from(self.entry.2) - inset
                && self.y > f64::from(self.entry.1) + inset
                && self.y < f64::from(self.entry.3) - inset
            {
                self.fired = false;
            }
            return None;
        }

        // The hand-off requires BOTH: the real cursor reached the entry edge
        // (snapped position in the edge band) AND the user's motion pushed past
        // it. The band check prevents a false fire when the cursor is pinned in
        // a hole between monitors while the invisible push overruns the layout.
        let (x0, y0, x1, y1) = self.entry;
        let near_edge = match self.edge {
            PointerEdge::Left => x <= f64::from(x0) + 1.0,
            PointerEdge::Right => x >= f64::from(x1) - 2.0,
            PointerEdge::Top => y <= f64::from(y0) + 1.0,
            PointerEdge::Bottom => y >= f64::from(y1) - 2.0,
        };
        let past_edge = match self.edge {
            PointerEdge::Left => raw_x < f64::from(x0) - RETURN_MARGIN_PX,
            PointerEdge::Right => raw_x > f64::from(x1) + RETURN_MARGIN_PX,
            PointerEdge::Top => raw_y < f64::from(y0) - RETURN_MARGIN_PX,
            PointerEdge::Bottom => raw_y > f64::from(y1) + RETURN_MARGIN_PX,
        };
        self.x = x;
        self.y = y;
        if near_edge && past_edge {
            self.fired = true;
            // Snap to the edge so reversing the push moves the cursor again
            // immediately (the overrun distance must not be swallowed).
            match self.edge {
                PointerEdge::Left => self.x = f64::from(x0),
                PointerEdge::Right => self.x = f64::from(x1) - 1.0,
                PointerEdge::Top => self.y = f64::from(y0),
                PointerEdge::Bottom => self.y = f64::from(y1) - 1.0,
            }
            return Some(self.edge);
        }
        None
    }

    /// The position to send to the compositor.
    ///
    /// Absolute events are only valid inside a region (half-open bounds); a
    /// position in a gap between monitors is clamped into the nearest region.
    fn absolute(&self) -> (f32, f32) {
        (self.x as f32, self.y as f32)
    }

    /// Snap a layout position into the region that contains it, or the
    /// nearest one (used both for detection bookkeeping and the sent value).
    fn snap(&self, x: f64, y: f64) -> (f64, f64) {
        for region in &self.regions {
            let (x0, y0, x1, y1) = *region;
            let max_x = f64::from(x1) - 1.0;
            let max_y = f64::from(y1) - 1.0;
            if x >= f64::from(x0) && x <= max_x && y >= f64::from(y0) && y <= max_y {
                return (x, y);
            }
        }
        let mut best: Option<((f64, f64), f64)> = None;
        for region in &self.regions {
            let (x0, y0, x1, y1) = *region;
            let cx = x.clamp(f64::from(x0), f64::from(x1) - 1.0);
            let cy = y.clamp(f64::from(y0), f64::from(y1) - 1.0);
            let distance = (cx - x) * (cx - x) + (cy - y) * (cy - y);
            if best
                .as_ref()
                .is_none_or(|(_, best_distance)| distance < *best_distance)
            {
                best = Some(((cx, cy), distance));
            }
        }
        best.map(|(position, _)| position)
            .unwrap_or((f64::from(self.layout_min_x), f64::from(self.layout_min_y)))
    }
}

/// Convert EIS regions into (x0, y0, x1, y1) logical-pixel boxes.
/// Convert EIS regions into (x0, y0, x1, y1) logical-pixel boxes.
fn region_boxes(regions: &[Region]) -> Vec<(f32, f32, f32, f32)> {
    regions
        .iter()
        .filter(|region| region.width > 0 && region.height > 0)
        .map(|region| {
            (
                region.x as f32,
                region.y as f32,
                region.x.saturating_add(region.width) as f32,
                region.y.saturating_add(region.height) as f32,
            )
        })
        .collect()
}

/// Pick the region that touches the entry edge.
///
/// A union bounding box would include holes between non-contiguous monitors,
/// where the compositor silently drops absolute events. Anchoring and clamping
/// inside a single real region keeps the tracked position valid.
fn entry_region_bounds(
    regions: &[(f32, f32, f32, f32)],
    edge: PointerEdge,
) -> Option<(f32, f32, f32, f32)> {
    regions.iter().copied().min_by(|a, b| {
        let key = |r: &(f32, f32, f32, f32)| match edge {
            PointerEdge::Left => r.0,
            PointerEdge::Right => -r.2,
            PointerEdge::Top => r.1,
            PointerEdge::Bottom => -r.3,
        };
        key(a).total_cmp(&key(b))
    })
}

async fn get_ei_fd() -> Result<(RemoteDesktop, Session<RemoteDesktop>, OwnedFd), ashpd::Error> {
    let remote_desktop = RemoteDesktop::new().await?;

    let restore_token = read_named_token(REMOTE_DESKTOP_TOKEN_FILE);
    log::debug!(
        "RemoteDesktop restore token: {}",
        if restore_token.is_some() {
            "present (the portal should skip its dialog)"
        } else {
            "missing (the portal will ask for permission)"
        }
    );

    log::debug!("creating session ...");
    let session = remote_desktop.create_session(Default::default()).await?;

    log::debug!("selecting devices ...");
    let options = SelectDevicesOptions::default()
        .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(restore_token.as_deref());
    remote_desktop.select_devices(&session, options).await?;

    log::info!("requesting permission for input emulation");
    let start_response = remote_desktop
        .start(&session, None, Default::default())
        .await?
        .response()?;

    // The restore token is only valid once, we need to re-save it each time
    if let Some(token_str) = start_response.restore_token() {
        if let Err(e) = write_named_token(REMOTE_DESKTOP_TOKEN_FILE, token_str) {
            log::warn!("failed to save RemoteDesktop token: {}", e);
        }
    }

    let fd = remote_desktop
        .connect_to_eis(&session, Default::default())
        .await?;
    Ok((remote_desktop, session, fd))
}

impl LibeiEmulation {
    /// Warp the pointer to the centre of the EIS region and start tracking the
    /// injected deltas from that known anchor.
    ///
    /// With absolute injection the tracked position is the real cursor
    /// position, so crossing the entry edge can be detected exactly - this is
    /// what makes the automatic return hand-off work on Wayland, where the
    /// compositor does not report capture barriers for injected motion.
    fn start_absolute_tracking(&mut self) -> bool {
        let Some(edge) = self.remote_edge else {
            return false;
        };
        let absolute = self
            .devices
            .absolute
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(pointer) = absolute else {
            if !self.absolute_warned {
                self.absolute_warned = true;
                log::warn!(
                    "peer drives this machine through {edge:?} but the compositor has no absolute pointer; automatic edge return is disabled - use the release bind to take control back"
                );
            }
            return false;
        };
        let paused = self
            .devices
            .paused
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&pointer.device);
        if paused {
            log::debug!("absolute pointer paused by the compositor; waiting for resume");
            return false;
        }
        let entry = entry_region_bounds(&pointer.regions, edge);
        let entry_scale = entry
            .and_then(|entry| {
                pointer
                    .regions
                    .iter()
                    .zip(pointer.scales.iter())
                    .find(|(region, _)| **region == entry)
                    .map(|(_, scale)| f64::from(*scale))
            })
            .filter(|scale| scale.is_finite() && *scale > 0.05)
            .unwrap_or(1.0);
        // user override for fine tuning without recompiling
        let user_scale = std::env::var("LAN_MOUSE_ABSOLUTE_SCALE")
            .ok()
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value > 0.05 && *value < 20.0)
            .unwrap_or(1.0);
        let delta_scale = (1.0 / entry_scale).clamp(0.2, 5.0) * user_scale;
        let Some(tracker) = AbsoluteTracker::new(
            edge,
            pointer.generation,
            pointer.regions.clone(),
            delta_scale,
        ) else {
            log::warn!("absolute pointer has no usable region; automatic edge return disabled");
            return false;
        };
        let (x, y) = tracker.absolute();
        pointer.pointer.motion_absolute(x, y);
        pointer.device.frame(self.conn.serial(), self.now_micros());
        if let Err(e) = self.context.flush() {
            log::warn!("failed to flush the absolute pointer warp: {e}");
        }
        let (min_x, min_y, max_x, max_y) = tracker.entry_bounds();
        log::info!(
            "absolute pointer armed at ({x},{y}) region {min_x},{min_y}..{max_x},{max_y} scale {delta_scale:.3}; push the cursor past the {edge:?} edge to hand control back"
        );
        self.absolute = Some(tracker);
        true
    }

    /// Monotonic microseconds for EI frame timestamps.
    fn now_micros(&self) -> u64 {
        self.clock.elapsed().as_micros() as u64
    }

    pub(crate) async fn new() -> Result<Self, LibeiEmulationCreationError> {
        let (_remote_desktop, session, eifd) = get_ei_fd().await?;
        let stream = UnixStream::from(eifd);
        stream.set_nonblocking(true)?;
        let context = ei::Context::new(stream)?;
        let (conn, events) = context
            .handshake_tokio("de.feschber.LanMouse", ContextType::Sender)
            .await?;
        let devices = Devices::default();
        let libei_error = Arc::new(AtomicBool::default());
        let error = Arc::new(Mutex::new(None));
        let ei_handler = ei_task(
            events,
            conn.clone(),
            context.clone(),
            devices.clone(),
            libei_error.clone(),
            error.clone(),
        );
        let ei_task = tokio::task::spawn_local(ei_handler);

        Ok(Self {
            context,
            conn,
            devices,
            ei_task,
            error,
            libei_error,
            _remote_desktop,
            session,
            remote_edge: None,
            absolute: None,
            edge_return: None,
            absolute_warned: false,
            clock: Instant::now(),
        })
    }
}

impl Drop for LibeiEmulation {
    fn drop(&mut self) {
        self.ei_task.abort();
    }
}

#[async_trait]
impl Emulation for LibeiEmulation {
    fn set_edge_return_notifier(&mut self, notifier: EdgeReturnNotifier) {
        self.edge_return = Some(notifier);
    }

    async fn begin_remote_control(&mut self, edge: PointerEdge) {
        self.remote_edge = Some(edge);
        self.absolute = None;
        self.absolute_warned = false;
        self.start_absolute_tracking();
    }

    async fn end_remote_control(&mut self) {
        self.remote_edge = None;
        self.absolute = None;
        self.absolute_warned = false;
    }

    async fn consume(
        &mut self,
        event: Event,
        _handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        // EI frame timestamps must be CLOCK_MONOTONIC microseconds.
        let now = self.now_micros();
        if self.libei_error.load(Ordering::SeqCst) {
            // don't break sending additional events but signal error
            let error = self.error.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(e) = error {
                return Err(e);
            }
        }
        match event {
            Event::Pointer(p) => match p {
                PointerEvent::Motion { time: _, dx, dy } => {
                    // A non-finite delta (corrupted packet) must not be
                    // forwarded into the EIS device state.
                    if !dx.is_finite() || !dy.is_finite() {
                        log::warn!("dropping non-finite motion: ({dx}, {dy})");
                        return Ok(());
                    }
                    // While a peer drives this machine and the compositor
                    // offers an absolute pointer, move it absolutely from a
                    // tracked position: that is exact (no acceleration/drift)
                    // and makes the edge-return detection trustworthy.
                    if self.remote_edge.is_some() {
                        if self.absolute.is_none() {
                            self.start_absolute_tracking();
                        }
                        // Only use the absolute device the tracker was armed
                        // against. A replaced/removed device falls back to
                        // relative injection (and re-arms on the next event).
                        let current = self
                            .devices
                            .absolute
                            .read()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .clone();
                        let usable = match (&self.absolute, current) {
                            (Some(tracker), Some(pointer))
                                if tracker.generation == pointer.generation =>
                            {
                                let paused = self
                                    .devices
                                    .paused
                                    .read()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                                    .contains(&pointer.device);
                                (!paused).then_some(pointer)
                            }
                            _ => None,
                        };
                        if let Some(pointer) = usable {
                            let mut tracker = self.absolute.take().expect("checked above");
                            let fired = tracker.apply(dx, dy);
                            let (x, y) = tracker.absolute();
                            pointer.pointer.motion_absolute(x, y);
                            pointer.device.frame(self.conn.serial(), now);
                            self.context
                                .flush()
                                .map_err(|e| io::Error::new(e.kind(), e))?;
                            self.absolute = Some(tracker);
                            if let Some(edge) = fired {
                                log::info!("pushed past the {edge:?} edge; handing control back");
                                if let Some(notifier) = &self.edge_return {
                                    let _ = notifier.send(edge);
                                }
                            }
                            return Ok(());
                        }
                        // device changed/paused: drop the stale tracker and
                        // keep injecting relatively until it is re-armed.
                        self.absolute = None;
                    }
                    // The wire carries f64 while EIS uses f32: a huge finite
                    // value would become infinity when narrowed.
                    let (dx, dy) = (dx as f32, dy as f32);
                    if !dx.is_finite() || !dy.is_finite() {
                        log::warn!("dropping out-of-range motion: ({dx}, {dy})");
                        return Ok(());
                    }
                    let pointer_device = self
                        .devices
                        .pointer
                        .read()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some((d, p)) = pointer_device.as_ref() {
                        p.motion_relative(dx, dy);
                        d.frame(self.conn.serial(), now);
                    }
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => {
                    let button_device = self
                        .devices
                        .button
                        .read()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some((d, b)) = button_device.as_ref() {
                        b.button(
                            button,
                            match state {
                                0 => ButtonState::Released,
                                _ => ButtonState::Press,
                            },
                        );
                        d.frame(self.conn.serial(), now);
                    }
                }
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => {
                    let Some((dx, dy)) = scroll_deltas(axis, value) else {
                        return Ok(());
                    };
                    let scroll_device = self
                        .devices
                        .scroll
                        .read()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some((d, s)) = scroll_device.as_ref() {
                        s.scroll(dx, dy);
                        d.frame(self.conn.serial(), now);
                    }
                }
                PointerEvent::AxisDiscrete120 { axis, value } => {
                    let Some((dx, dy)) = discrete_scroll_deltas(axis, value) else {
                        return Ok(());
                    };
                    let scroll_device = self
                        .devices
                        .scroll
                        .read()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some((d, s)) = scroll_device.as_ref() {
                        s.scroll_discrete(dx, dy);
                        d.frame(self.conn.serial(), now);
                    }
                }
            },
            Event::Keyboard(k) => match k {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    let keyboard_device = self
                        .devices
                        .keyboard
                        .read()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some((d, k)) = keyboard_device.as_ref() {
                        k.key(
                            key,
                            match state {
                                0 => KeyState::Released,
                                _ => KeyState::Press,
                            },
                        );
                        d.frame(self.conn.serial(), now);
                    }
                }
                KeyboardEvent::Modifiers { .. } => {}
            },
        }
        self.context
            .flush()
            .map_err(|e| io::Error::new(e.kind(), e))?;
        Ok(())
    }

    async fn create(&mut self, _: EmulationHandle) {}
    async fn destroy(&mut self, _: EmulationHandle) {}

    async fn terminate(&mut self) {
        // A hung portal must not hang app shutdown: close on a best-effort
        // basis with a timeout, then abort the EIS task.
        match tokio::time::timeout(Duration::from_secs(2), self.session.close()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::warn!("session.close(): {e}"),
            Err(_) => log::warn!("session.close() timed out"),
        }
        self.ei_task.abort();
    }
}

/// Map a continuous scroll event to EIS (dx, dy) deltas.
///
/// Only the two axes the wire protocol defines are accepted; an unknown axis
/// from a corrupted packet is dropped instead of being treated as horizontal.
fn scroll_deltas(axis: u8, value: f64) -> Option<(f32, f32)> {
    if !value.is_finite() {
        return None;
    }
    let value = value as f32;
    // Narrowing can turn a huge-but-finite f64 (e.g. 1e300 from a corrupted
    // packet) into f32::INFINITY; EIS must never receive a non-finite delta.
    if !value.is_finite() {
        return None;
    }
    match axis {
        0 => Some((0.0, value)),
        1 => Some((value, 0.0)),
        _ => None,
    }
}

/// Map a discrete (120-unit) scroll event to EIS (dx, dy) deltas.
fn discrete_scroll_deltas(axis: u8, value: i32) -> Option<(i32, i32)> {
    match axis {
        0 => Some((0, value)),
        1 => Some((value, 0)),
        _ => None,
    }
}

async fn ei_task(
    mut events: EiConvertEventStream,
    conn: Connection,
    context: ei::Context,
    devices: Devices,
    libei_error: Arc<AtomicBool>,
    error: Arc<Mutex<Option<EmulationError>>>,
) {
    loop {
        match ei_event_handler(&mut events, &conn, &context, &devices).await {
            Ok(()) => {}
            Err(e) => {
                libei_error.store(true, Ordering::SeqCst);
                error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .replace(e);
                // wait for termination -> otherwise we will loop forever
                future::pending::<()>().await;
            }
        }
    }
}

async fn ei_event_handler(
    events: &mut EiConvertEventStream,
    conn: &Connection,
    context: &ei::Context,
    devices: &Devices,
) -> Result<(), EmulationError> {
    loop {
        let event = events.next().await.ok_or(EmulationError::EndOfStream)??;
        let capabilities = DeviceCapability::Pointer
            | DeviceCapability::PointerAbsolute
            | DeviceCapability::Keyboard
            | DeviceCapability::Touch
            | DeviceCapability::Scroll
            | DeviceCapability::Button;
        log::debug!("{event:?}");
        match event {
            EiEvent::Disconnected(e) => {
                log::debug!("ei disconnected: {e:?}");
                return Err(EmulationError::EndOfStream);
            }
            EiEvent::SeatAdded(e) => {
                e.seat().bind_capabilities(capabilities);
            }
            EiEvent::SeatRemoved(e) => {
                log::debug!("seat removed: {:?}", e.seat());
            }
            EiEvent::DeviceAdded(e) => {
                let device_type = e.device().device_type();
                log::debug!("device added: {device_type:?}");
                let device = e.device();
                if let Some(pointer) = e.device().interface::<Pointer>() {
                    devices
                        .pointer
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .replace((device.device().clone(), pointer));
                }
                if let Some(pointer) = e.device().interface::<PointerAbsolute>() {
                    let regions = region_boxes(e.device().regions());
                    if regions.is_empty() {
                        log::warn!(
                            "absolute pointer device has no usable region; automatic edge return disabled"
                        );
                    } else {
                        let scales: Vec<f32> = e
                            .device()
                            .regions()
                            .iter()
                            .filter(|region| region.width > 0 && region.height > 0)
                            .map(|region| region.scale)
                            .collect();
                        let generation = devices
                            .absolute_generation
                            .fetch_add(1, Ordering::SeqCst)
                            .wrapping_add(1);
                        log::info!(
                            "absolute pointer available ({} region(s)); automatic edge return enabled",
                            regions.len()
                        );
                        devices
                            .absolute
                            .write()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .replace(AbsolutePointer {
                                device: device.device().clone(),
                                pointer,
                                generation,
                                regions,
                                scales,
                            });
                    }
                }
                if let Some(keyboard) = e.device().interface::<Keyboard>() {
                    devices
                        .keyboard
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .replace((device.device().clone(), keyboard));
                }
                if let Some(scroll) = e.device().interface::<Scroll>() {
                    devices
                        .scroll
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .replace((device.device().clone(), scroll));
                }
                if let Some(button) = e.device().interface::<Button>() {
                    devices
                        .button
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .replace((device.device().clone(), button));
                }
            }
            EiEvent::DeviceRemoved(e) => {
                log::debug!("device removed: {:?}", e.device().device_type());
                // Never send events on the destroyed EIS objects again; a
                // fresh interface set arrives with the next DeviceAdded.
                devices.forget_device(e.device().device());
            }
            EiEvent::DevicePaused(e) => {
                log::debug!("device paused: {:?}", e.device().device_type());
                devices
                    .paused
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(e.device().device().clone());
            }
            EiEvent::DeviceResumed(e) => {
                log::debug!("device resumed: {:?}", e.device().device_type());
                devices
                    .paused
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(e.device().device());
                let sequence = devices
                    .resume_sequence
                    .fetch_add(1, Ordering::SeqCst)
                    .wrapping_add(1);
                e.device()
                    .device()
                    .start_emulating(conn.serial(), sequence as u32);
            }
            EiEvent::KeyboardModifiers(e) => {
                log::debug!("modifiers: {e:?}");
            }
            // only for receiver context
            // EiEvent::Frame(_) => { },
            // EiEvent::DeviceStartEmulating(_) => { },
            // EiEvent::DeviceStopEmulating(_) => { },
            // EiEvent::PointerMotion(_) => { },
            // EiEvent::PointerMotionAbsolute(_) => { },
            // EiEvent::Button(_) => { },
            // EiEvent::ScrollDelta(_) => { },
            // EiEvent::ScrollStop(_) => { },
            // EiEvent::ScrollCancel(_) => { },
            // EiEvent::ScrollDiscrete(_) => { },
            // EiEvent::KeyboardKey(_) => { },
            // EiEvent::TouchDown(_) => { },
            // EiEvent::TouchUp(_) => { },
            // EiEvent::TouchMotion(_) => { },
            other => {
                // Receiver-only or unknown events must never panic the
                // service: an unexpected protocol event is not worth a crash.
                log::debug!("ignoring unexpected eis event: {other:?}");
            }
        }
        context.flush().map_err(|e| io::Error::new(e.kind(), e))?;
    }
}

#[cfg(test)]
mod absolute_tracker_tests {
    use super::*;

    const W: f32 = 2560.0;
    const H: f32 = 1600.0;

    fn tracker(edge: PointerEdge) -> AbsoluteTracker {
        AbsoluteTracker::new(edge, 1, vec![(0.0, 0.0, W, H)], 1.0).expect("single region")
    }

    fn dual_regions() -> Vec<(f32, f32, f32, f32)> {
        vec![(0.0, 0.0, 1920.0, 1080.0), (1920.0, 0.0, 3840.0, 1080.0)]
    }

    #[test]
    fn anchor_starts_at_the_entry_edge() {
        assert_eq!(tracker(PointerEdge::Left).absolute(), (24.0, 800.0));
        assert_eq!(
            tracker(PointerEdge::Right).absolute(),
            (W - 1.0 - 24.0, 800.0)
        );
        assert_eq!(tracker(PointerEdge::Top).absolute(), (1280.0, 24.0));
        assert_eq!(
            tracker(PointerEdge::Bottom).absolute(),
            (1280.0, H - 1.0 - 24.0)
        );
    }

    #[test]
    fn moving_inside_the_screen_never_returns() {
        let mut tracker = tracker(PointerEdge::Left);
        assert_eq!(tracker.apply(100.0, 0.0), None);
        assert_eq!(tracker.apply(50.0, 40.0), None);
        assert_eq!(tracker.apply(-80.0, -40.0), None);
        assert_eq!(tracker.apply(-40.0, 0.0), None);
        assert!(!tracker.fired);
    }

    #[test]
    fn pushing_past_the_entry_edge_fires_exactly_once() {
        let mut tracker = tracker(PointerEdge::Left);
        // anchored 24 px inside; a push back past the edge hands control back
        assert_eq!(tracker.apply(-30.0, 0.0), Some(PointerEdge::Left));
        // latched: a single hand-off per activation
        assert_eq!(tracker.apply(-50.0, 0.0), None);
    }

    #[test]
    fn every_edge_uses_its_own_axis_and_direction() {
        let cases = [
            (PointerEdge::Left, -30.0, 0.0),
            (PointerEdge::Right, 30.0, 0.0),
            (PointerEdge::Top, 0.0, -30.0),
            (PointerEdge::Bottom, 0.0, 30.0),
        ];
        for (edge, sx, sy) in cases {
            let mut tracker = tracker(edge);
            assert_eq!(tracker.apply(sx, sy), Some(edge), "{edge:?}");
        }
    }

    #[test]
    fn wrong_axis_does_not_fire() {
        let mut tracker = tracker(PointerEdge::Left);
        assert_eq!(tracker.apply(0.0, -800.0), None);
        assert_eq!(tracker.apply(0.0, 800.0), None);
        assert!(!tracker.fired);
    }

    #[test]
    fn absolute_position_stays_valid_in_half_open_region_bounds() {
        let mut tracker = tracker(PointerEdge::Right);
        assert_eq!(tracker.apply(30.0, 0.0), Some(PointerEdge::Right));
        let (x, y) = tracker.absolute();
        assert!((0.0..=W - 1.0).contains(&x));
        assert!((0.0..=H - 1.0).contains(&y));
    }

    #[test]
    fn pointer_can_cross_into_the_adjacent_monitor() {
        let mut tracker = AbsoluteTracker::new(PointerEdge::Left, 1, dual_regions(), 1.0).unwrap();
        // anchored at the left edge of the first monitor
        assert_eq!(tracker.absolute(), (24.0, 540.0));
        assert_eq!(tracker.apply(2000.0, 0.0), None);
        let (x, _) = tracker.absolute();
        assert!(x > 1920.0, "cursor must reach the second monitor, got {x}");
        assert!(x <= 3839.0);
    }

    #[test]
    fn pointer_can_cross_into_a_stacked_monitor() {
        let regions = vec![(0.0, 0.0, 1920.0, 1080.0), (0.0, 1080.0, 1920.0, 2160.0)];
        let mut tracker = AbsoluteTracker::new(PointerEdge::Top, 1, regions, 1.0).unwrap();
        assert_eq!(tracker.apply(0.0, 1500.0), None);
        let (_, y) = tracker.absolute();
        assert!(y > 1080.0, "cursor must reach the lower monitor, got {y}");
    }

    #[test]
    fn overrun_does_not_swallow_the_reverse_movement() {
        let mut tracker = tracker(PointerEdge::Left);
        assert_eq!(tracker.apply(-30.0, 0.0), Some(PointerEdge::Left));
        // cursor snapped to the edge; moving back inside moves it immediately
        let _ = tracker.apply(500.0, 0.0);
        let (x, _) = tracker.absolute();
        assert!((x - 500.0).abs() < 1.0, "expected x≈500, got {x}");
    }

    #[test]
    fn implausible_single_delta_is_dropped() {
        let mut tracker = tracker(PointerEdge::Left);
        let before = tracker.absolute();
        assert_eq!(tracker.apply(-1.0e12, 0.0), None);
        assert!(!tracker.fired);
        assert_eq!(tracker.absolute(), before);
    }

    #[test]
    fn hidpi_scale_normalises_the_physical_speed() {
        // display scale 1.6667: 1000 physical-ish counts move the logical
        // cursor 600 px, matching the previous relative injection feel
        let mut tracker = AbsoluteTracker::new(
            PointerEdge::Left,
            1,
            vec![(0.0, 0.0, 1536.0, 960.0)],
            1.0 / 1.66667,
        )
        .expect("region");
        let (x0, _) = tracker.absolute();
        assert_eq!(tracker.apply(1000.0, 0.0), None);
        let (x1, _) = tracker.absolute();
        assert!(
            (x1 - x0 - 600.0).abs() < 2.0,
            "expected ~600 px, got {}",
            x1 - x0
        );
    }

    #[test]
    fn hole_between_monitors_does_not_fire_a_false_handoff() {
        // L-shaped layout: A(0,0,1920x1080), B(1920,0,1920x1080),
        // C(3840,1080,1920x1080); there is an empty lower-left quadrant.
        let regions = vec![
            (0.0, 0.0, 1920.0, 1080.0),
            (1920.0, 0.0, 3840.0, 1080.0),
            (3840.0, 1080.0, 5760.0, 2160.0),
        ];
        let mut tracker =
            AbsoluteTracker::new(PointerEdge::Bottom, 1, regions, 1.0).expect("regions");
        // anchor is at the bottom edge of C
        assert_eq!(tracker.absolute(), (4800.0, 2135.0));
        // push left into the empty quadrant: the real cursor is snapped to a
        // monitor edge, so this must NOT hand off
        assert_eq!(tracker.apply(-3000.0, 0.0), None);
        assert!(!tracker.fired);
        // more vertical push while still in the hole: still no hand-off
        assert_eq!(tracker.apply(0.0, 600.0), None);
        assert!(!tracker.fired);
        // reach C again, then genuinely push past its bottom edge
        let _ = tracker.apply(2500.0, 0.0);
        let _ = tracker.apply(0.0, 500.0);
        assert_eq!(tracker.apply(0.0, 700.0), Some(PointerEdge::Bottom));
    }

    #[test]
    fn entry_region_is_the_monitor_touching_that_edge() {
        let regions = vec![
            Region {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale: 1.0,
                mapping_id: None,
            },
            Region {
                x: 1920,
                y: 0,
                width: 1280,
                height: 720,
                scale: 1.0,
                mapping_id: None,
            },
        ];
        let boxes = region_boxes(&regions);
        assert_eq!(boxes.len(), 2);
        assert_eq!(
            entry_region_bounds(&boxes, PointerEdge::Left),
            Some((0.0, 0.0, 1920.0, 1080.0))
        );
        assert_eq!(
            entry_region_bounds(&boxes, PointerEdge::Right),
            Some((1920.0, 0.0, 3200.0, 720.0))
        );
        assert_eq!(entry_region_bounds(&[], PointerEdge::Left), None);
    }

    #[test]
    fn fired_latch_rearms_after_returning_inside() {
        let mut tracker = tracker(PointerEdge::Left);
        assert_eq!(tracker.apply(-30.0, 0.0), Some(PointerEdge::Left));
        assert_eq!(tracker.apply(-8.0, 0.0), None);
        let _ = tracker.apply(200.0, 0.0);
        assert_eq!(tracker.apply(-1200.0, 0.0), Some(PointerEdge::Left));
    }
}
