use futures::{StreamExt, future};
use std::{
    io,
    os::{fd::OwnedFd, unix::net::UnixStream},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
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
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
}

#[derive(Clone, Default)]
struct Devices {
    pointer: Arc<RwLock<Option<(ei::Device, ei::Pointer)>>>,
    /// set when the compositor exposes an absolute pointer device (libei
    /// PI_ABSOLUTE): lets us position the pointer exactly and detect the
    /// return hand-off at the entry edge
    absolute: Arc<RwLock<Option<AbsolutePointer>>>,
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
}

/// Pixels the pointer must be pushed past the entry edge before the hand-off
/// fires. The absolute tracker knows the exact position, so this can be tiny.
const RETURN_MARGIN_PX: f64 = 3.0;

fn ei_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

/// Exact edge-return detector for absolute pointer injection.
///
/// The pointer is warped to a known anchor on entry and every injected motion
/// delta updates the tracked position. Because the same position is sent to
/// the compositor as an absolute motion event, the tracked position *is* the
/// real cursor position: crossing the entry edge is unambiguous.
#[derive(Clone, Copy, Debug)]
struct AbsoluteTracker {
    edge: PointerEdge,
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
    x: f64,
    y: f64,
    fired: bool,
}

impl AbsoluteTracker {
    fn new(edge: PointerEdge, min_x: f32, min_y: f32, max_x: f32, max_y: f32) -> Self {
        Self {
            edge,
            min_x,
            min_y,
            max_x,
            max_y,
            x: f64::from(min_x + (max_x - min_x) / 2.0),
            y: f64::from(min_y + (max_y - min_y) / 2.0),
            fired: false,
        }
    }

    /// Apply one injected delta and report the hand-off edge once.
    fn apply(&mut self, dx: f64, dy: f64) -> Option<PointerEdge> {
        self.x += dx;
        self.y += dy;
        if self.fired {
            return None;
        }
        let past_edge = match self.edge {
            PointerEdge::Left => self.x < f64::from(self.min_x) - RETURN_MARGIN_PX,
            PointerEdge::Right => self.x > f64::from(self.max_x) + RETURN_MARGIN_PX,
            PointerEdge::Top => self.y < f64::from(self.min_y) - RETURN_MARGIN_PX,
            PointerEdge::Bottom => self.y > f64::from(self.max_y) + RETURN_MARGIN_PX,
        };
        if past_edge {
            self.fired = true;
            return Some(self.edge);
        }
        None
    }

    /// The position to send to the compositor (clamped into the region: EIS
    /// silently drops out-of-region absolute motion).
    fn absolute(&self) -> (f32, f32) {
        (
            self.x.clamp(f64::from(self.min_x), f64::from(self.max_x)) as f32,
            self.y.clamp(f64::from(self.min_y), f64::from(self.max_y)) as f32,
        )
    }
}

/// Union of the device regions, used as the absolute coordinate space.
fn region_bounds(regions: &[Region]) -> Option<(f32, f32, f32, f32)> {
    let mut iter = regions.iter().filter(|r| r.width > 0 && r.height > 0);
    let first = iter.next()?;
    let mut min_x = first.x as f32;
    let mut min_y = first.y as f32;
    let mut max_x = first.x.saturating_add(first.width) as f32;
    let mut max_y = first.y.saturating_add(first.height) as f32;
    for region in iter {
        min_x = min_x.min(region.x as f32);
        min_y = min_y.min(region.y as f32);
        max_x = max_x.max(region.x.saturating_add(region.width) as f32);
        max_y = max_y.max(region.y.saturating_add(region.height) as f32);
    }
    Some((min_x, min_y, max_x, max_y))
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
            log::warn!(
                "peer drives this machine through {edge:?} but the compositor has no absolute pointer; automatic edge return is disabled - use the release bind to take control back"
            );
            return false;
        };
        let tracker = AbsoluteTracker::new(
            edge,
            pointer.min_x,
            pointer.min_y,
            pointer.max_x,
            pointer.max_y,
        );
        let (x, y) = tracker.absolute();
        pointer.pointer.motion_absolute(x, y);
        pointer.device.frame(self.conn.serial(), ei_time());
        if let Err(e) = self.context.flush() {
            log::warn!("failed to flush the absolute pointer warp: {e}");
        }
        log::info!(
            "absolute pointer armed at ({x},{y}); push the cursor past the {edge:?} edge to hand control back"
        );
        self.absolute = Some(tracker);
        true
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
        self.start_absolute_tracking();
    }

    async fn end_remote_control(&mut self) {
        self.remote_edge = None;
        self.absolute = None;
    }

    async fn consume(
        &mut self,
        event: Event,
        _handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
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
                        if let Some(tracker) = self.absolute.as_mut() {
                            let fired = tracker.apply(dx, dy);
                            let (x, y) = tracker.absolute();
                            let absolute = self
                                .devices
                                .absolute
                                .read()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .clone();
                            if let Some(pointer) = absolute {
                                pointer.pointer.motion_absolute(x, y);
                                pointer.device.frame(self.conn.serial(), now);
                                self.context
                                    .flush()
                                    .map_err(|e| io::Error::new(e.kind(), e))?;
                            }
                            if let Some(edge) = fired {
                                log::info!("pushed past the {edge:?} edge; handing control back");
                                if let Some(notifier) = &self.edge_return {
                                    let _ = notifier.send(edge);
                                }
                            }
                            return Ok(());
                        }
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
    _conn: Connection,
    context: ei::Context,
    devices: Devices,
    libei_error: Arc<AtomicBool>,
    error: Arc<Mutex<Option<EmulationError>>>,
) {
    loop {
        match ei_event_handler(&mut events, &context, &devices).await {
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
                    match region_bounds(e.device().regions()) {
                        Some((min_x, min_y, max_x, max_y)) => {
                            log::info!(
                                "absolute pointer available: EIS region {min_x},{min_y}..{max_x},{max_y}; automatic edge return enabled"
                            );
                            devices
                                .absolute
                                .write()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .replace(AbsolutePointer {
                                    device: device.device().clone(),
                                    pointer,
                                    min_x,
                                    min_y,
                                    max_x,
                                    max_y,
                                });
                        }
                        None => log::warn!(
                            "absolute pointer device has no usable region; automatic edge return disabled"
                        ),
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
            }
            EiEvent::DeviceResumed(e) => {
                log::debug!("device resumed: {:?}", e.device().device_type());
                e.device().device().start_emulating(0, 0);
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
        AbsoluteTracker::new(edge, 0.0, 0.0, W, H)
    }

    #[test]
    fn moving_inside_the_screen_never_returns() {
        let mut tracker = tracker(PointerEdge::Left);
        assert_eq!(tracker.apply(100.0, 0.0), None);
        assert_eq!(tracker.apply(50.0, 40.0), None);
        assert_eq!(tracker.apply(-80.0, -40.0), None);
        assert_eq!(tracker.apply(-200.0, 0.0), None);
        assert!(!tracker.fired);
    }

    #[test]
    fn pushing_past_the_entry_edge_fires_exactly_once() {
        let mut tracker = tracker(PointerEdge::Left);
        // walk from the centre to the entry edge (x == 0)
        assert_eq!(tracker.apply(-1280.0, 0.0), None);
        // a small overshoot is not enough
        assert_eq!(tracker.apply(-2.0, 0.0), None);
        // past the margin: hand the control back
        assert_eq!(tracker.apply(-2.0, 0.0), Some(PointerEdge::Left));
        // latched: a single hand-off per activation
        assert_eq!(tracker.apply(-50.0, 0.0), None);
    }

    #[test]
    fn every_edge_uses_its_own_axis_and_direction() {
        let cases = [
            (PointerEdge::Left, -1280.0, 0.0, -8.0, 0.0),
            (PointerEdge::Right, 1280.0, 0.0, 8.0, 0.0),
            (PointerEdge::Top, 0.0, -800.0, 0.0, -8.0),
            (PointerEdge::Bottom, 0.0, 800.0, 0.0, 8.0),
        ];
        for (edge, dx, dy, sx, sy) in cases {
            let mut tracker = tracker(edge);
            // walk to that edge first
            let _ = tracker.apply(dx, dy);
            // pushing past the margin in that direction fires once
            assert_eq!(tracker.apply(sx, sy), Some(edge), "{edge:?}");
        }
    }

    #[test]
    fn wrong_axis_does_not_fire() {
        let mut tracker = tracker(PointerEdge::Left);
        assert_eq!(tracker.apply(-1280.0, 0.0), None);
        // moving only vertically at the edge is not a return
        assert_eq!(tracker.apply(0.0, -800.0), None);
        assert_eq!(tracker.apply(0.0, 800.0), None);
        assert!(!tracker.fired);
    }

    #[test]
    fn tracked_position_is_clamped_to_the_region_when_sent() {
        let mut tracker = tracker(PointerEdge::Right);
        assert_eq!(tracker.apply(5000.0, 5000.0), Some(PointerEdge::Right));
        let (x, y) = tracker.absolute();
        assert!((0.0..=W).contains(&x));
        assert!((0.0..=H).contains(&y));
    }

    #[test]
    fn region_bounds_use_the_union_of_regions() {
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
        assert_eq!(region_bounds(&regions), Some((0.0, 0.0, 3200.0, 1080.0)));
        assert_eq!(region_bounds(&[]), None);
    }
}
