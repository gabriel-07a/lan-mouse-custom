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
        self, Button, Keyboard, Pointer, Scroll, button::ButtonState, handshake::ContextType,
        keyboard::KeyState,
    },
    event::{self, Connection, DeviceCapability, DeviceEvent, EiEvent, SeatEvent},
    tokio::EiConvertEventStream,
};

use input_event::{Event, KeyboardEvent, PointerEvent};

use crate::{
    error::EmulationError,
    portal_token::{REMOTE_DESKTOP_TOKEN_FILE, read_named_token, write_named_token},
};

use super::{Emulation, EmulationHandle, error::LibeiEmulationCreationError};

#[derive(Clone, Default)]
struct Devices {
    pointer: Arc<RwLock<Option<(ei::Device, ei::Pointer)>>>,
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
