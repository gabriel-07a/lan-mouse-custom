use ashpd::{
    desktop::{
        PersistMode, Session,
        remote_desktop::{
            Axis, DeviceType, KeyState, NotifyPointerAxisOptions, RemoteDesktop,
            SelectDevicesOptions,
        },
    },
    zbus::AsyncDrop,
};
use std::time::Duration;

use async_trait::async_trait;

use futures::FutureExt;
use input_event::{
    Event::{Keyboard, Pointer},
    KeyboardEvent, PointerEvent,
};

use crate::{
    error::EmulationError,
    portal_token::{read_named_token, write_named_token},
};

use super::{Emulation, EmulationHandle, error::XdpEmulationCreationError};

/// Name of the restore token file of this backend.
///
/// Kept separate from the libei token ([`crate::portal_token::REMOTE_DESKTOP_TOKEN_FILE`]):
/// restore tokens are single-use and bound to the session that issued them, so
/// the two backends must not overwrite each other's consent.
const XDP_RESTORE_TOKEN_FILE: &str = "remote-desktop-xdp.token";

pub(crate) struct DesktopPortalEmulation {
    proxy: RemoteDesktop,
    session: Session<RemoteDesktop>,
}

impl DesktopPortalEmulation {
    pub(crate) async fn new() -> Result<DesktopPortalEmulation, XdpEmulationCreationError> {
        log::debug!("connecting to org.freedesktop.portal.RemoteDesktop portal ...");
        let proxy = RemoteDesktop::new().await?;

        log::debug!("creating session ...");
        let session = proxy.create_session(Default::default()).await?;

        // Reuse the persisted consent so the fallback backend does not prompt
        // on every app start (the token was previously discarded here).
        let restore_token = read_named_token(XDP_RESTORE_TOKEN_FILE);
        log::debug!(
            "RemoteDesktop restore token: {}",
            if restore_token.is_some() {
                "present (the portal should skip its dialog)"
            } else {
                "missing (the portal will ask for permission)"
            }
        );

        log::debug!("selecting devices ...");
        let options = SelectDevicesOptions::default()
            .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(restore_token.as_deref());
        proxy.select_devices(&session, options).await?;

        log::info!("requesting permission for input emulation");
        let devices = proxy
            .start(&session, None, Default::default())
            .await?
            .response()?;

        // The restore token is single-use: the portal returns the replacement
        // after every successful start and the old one stops working.
        if let Some(token) = devices.restore_token() {
            if let Err(e) = write_named_token(XDP_RESTORE_TOKEN_FILE, token) {
                log::warn!("failed to save RemoteDesktop token: {e}");
            }
        }

        // Warning only: some portal implementations report the device list
        // imprecisely, and refusing a working session would be worse than
        // dropping the occasional unmatched event.
        let granted = devices.devices();
        if !granted.contains(DeviceType::Keyboard) || !granted.contains(DeviceType::Pointer) {
            log::warn!(
                "the portal granted fewer devices than requested ({granted:?}); \
                 keyboard/pointer input may be dropped"
            );
        }

        log::debug!("started session");

        Ok(Self { proxy, session })
    }
}

/// Only the two axes the wire protocol defines are valid; a malformed axis
/// from a corrupted packet is dropped instead of being treated as horizontal.
fn portal_axis(axis: u8) -> Option<Axis> {
    match axis {
        0 => Some(Axis::Vertical),
        1 => Some(Axis::Horizontal),
        _ => None,
    }
}

#[async_trait]
impl Emulation for DesktopPortalEmulation {
    async fn consume(
        &mut self,
        event: input_event::Event,
        _client: EmulationHandle,
    ) -> Result<(), EmulationError> {
        match event {
            Pointer(p) => match p {
                PointerEvent::Motion { time: _, dx, dy } => {
                    // A corrupted packet can carry NaN/inf; it must not reach
                    // the portal (or be turned into a bogus cursor jump).
                    if !dx.is_finite() || !dy.is_finite() {
                        log::warn!("dropping non-finite motion: ({dx}, {dy})");
                        return Ok(());
                    }
                    self.proxy
                        .notify_pointer_motion(&self.session, dx, dy, Default::default())
                        .await?;
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => {
                    let Ok(button) = i32::try_from(button) else {
                        log::warn!("dropping out-of-range pointer button: {button}");
                        return Ok(());
                    };
                    let state = match state {
                        0 => KeyState::Released,
                        _ => KeyState::Pressed,
                    };
                    self.proxy
                        .notify_pointer_button(&self.session, button, state, Default::default())
                        .await?;
                }
                PointerEvent::AxisDiscrete120 { axis, value } => {
                    let Some(axis) = portal_axis(axis) else {
                        return Ok(());
                    };
                    self.proxy
                        .notify_pointer_axis_discrete(
                            &self.session,
                            axis,
                            value / 120,
                            Default::default(),
                        )
                        .await?;
                }
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => {
                    let Some(axis) = portal_axis(axis) else {
                        return Ok(());
                    };
                    if !value.is_finite() {
                        return Ok(());
                    }
                    let (dx, dy) = match axis {
                        Axis::Vertical => (0., value),
                        Axis::Horizontal => (value, 0.),
                    };
                    self.proxy
                        .notify_pointer_axis(
                            &self.session,
                            dx,
                            dy,
                            NotifyPointerAxisOptions::default().set_finish(true),
                        )
                        .await?;
                }
            },
            Keyboard(k) => {
                match k {
                    KeyboardEvent::Key {
                        time: _,
                        key,
                        state,
                    } => {
                        let Ok(keycode) = i32::try_from(key) else {
                            log::warn!("dropping out-of-range keycode: {key}");
                            return Ok(());
                        };
                        let state = match state {
                            0 => KeyState::Released,
                            _ => KeyState::Pressed,
                        };
                        self.proxy
                            .notify_keyboard_keycode(
                                &self.session,
                                keycode,
                                state,
                                Default::default(),
                            )
                            .await?;
                    }
                    KeyboardEvent::Modifiers { .. } => {
                        // ignore
                    }
                }
            }
        }
        Ok(())
    }

    async fn create(&mut self, _client: EmulationHandle) {}
    async fn destroy(&mut self, _client: EmulationHandle) {}
    async fn terminate(&mut self) {
        // Never let a hung portal keep the app's shutdown blocked.
        match tokio::time::timeout(Duration::from_secs(2), self.session.close()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::warn!("session.close(): {e}"),
            Err(_) => log::warn!("session.close() timed out"),
        }
        match tokio::time::timeout(Duration::from_secs(2), self.session.receive_closed()).await {
            // Subscribing to the closed stream cannot hang; keep the failure
            // logging of the previous implementation.
            Ok(Ok(_)) => {}
            Ok(Err(e)) => log::warn!("session.receive_closed(): {e}"),
            Err(_) => log::warn!("session.receive_closed() timed out"),
        }
    }
}

impl AsyncDrop for DesktopPortalEmulation {
    #[doc = r" Perform the async cleanup."]
    #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
    fn async_drop<'async_trait>(
        self,
    ) -> ::core::pin::Pin<
        Box<dyn ::core::future::Future<Output = ()> + ::core::marker::Send + 'async_trait>,
    >
    where
        Self: 'async_trait,
    {
        async move {
            let _ = tokio::time::timeout(Duration::from_secs(2), self.session.close()).await;
        }
        .boxed()
    }
}
