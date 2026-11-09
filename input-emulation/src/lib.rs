use async_trait::async_trait;
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};

use input_event::{Event, KeyboardEvent, PointerEvent};

pub use self::error::{EmulationCreationError, EmulationError, InputEmulationError};

#[cfg(windows)]
mod windows;

#[cfg(x11)]
mod x11;

#[cfg(wlroots)]
mod wlroots;

#[cfg(rdp)]
mod xdg_desktop_portal;

#[cfg(libei)]
mod libei;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(any(libei, rdp))]
mod portal_token;

mod absolute;
/// fallback input emulation (logs events)
mod dummy;
mod error;
mod subpixel;

pub type EmulationHandle = u64;

/// Whether a persisted RemoteDesktop consent token exists.
///
/// When no token is present the portal would show a permission dialog; the
/// service uses this to distinguish "we may start emulation automatically"
/// (consent already granted) from "only an explicit user action may ask".
pub fn remote_desktop_consent_present() -> bool {
    #[cfg(libei)]
    {
        crate::portal_token::read_named_token(crate::portal_token::REMOTE_DESKTOP_TOKEN_FILE)
            .is_some()
    }
    #[cfg(not(libei))]
    {
        false
    }
}

/// Whether starting this backend could show a portal permission dialog and
/// therefore needs persisted consent to start automatically.
pub fn backend_needs_portal_consent(backend: Option<Backend>) -> bool {
    #[cfg(libei)]
    {
        matches!(backend, None | Some(Backend::Libei))
    }
    #[cfg(not(libei))]
    {
        let _ = backend;
        false
    }
}

/// Forget the stored RemoteDesktop consent (a cancelled/failed portal request
/// invalidates it anyway; removing it prevents unsolicited dialogs on the next
/// start).
pub fn clear_remote_desktop_consent() {
    #[cfg(libei)]
    crate::portal_token::remove_named_token(crate::portal_token::REMOTE_DESKTOP_TOKEN_FILE);
}

/// Screen edge the remote pointer was pushed across while a peer drives this
/// machine.
///
/// The backend that injects the remote input is often the only component that
/// can observe the pointer being pushed past a screen edge: Windows injects
/// absolute positions (which the raw-input barrier detector ignores) and the
/// GNOME/libei capture portal does not reliably report barriers for injected
/// motion. Reporting the edge lets the service ask the peer to release its
/// capture — that is what hands the cursor back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerEdge {
    Left,
    Right,
    Top,
    Bottom,
}

/// Channel used by a backend to report [`PointerEdge`] return requests.
pub type EdgeReturnNotifier = tokio::sync::mpsc::UnboundedSender<PointerEdge>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(wlroots)]
    Wlroots,
    #[cfg(libei)]
    Libei,
    #[cfg(rdp)]
    Xdp,
    #[cfg(x11)]
    X11,
    #[cfg(windows)]
    Windows,
    #[cfg(target_os = "macos")]
    MacOs,
    Dummy,
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(wlroots)]
            Backend::Wlroots => write!(f, "wlroots"),
            #[cfg(libei)]
            Backend::Libei => write!(f, "libei"),
            #[cfg(rdp)]
            Backend::Xdp => write!(f, "xdg-desktop-portal"),
            #[cfg(x11)]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "macos"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

pub struct InputEmulation {
    emulation: Box<dyn Emulation>,
    backend: Backend,
    handles: HashSet<EmulationHandle>,
    pressed_keys: HashMap<EmulationHandle, HashSet<u32>>,
    /// Pointer buttons held by each handle. Like `pressed_keys`, they are
    /// released when the handle is destroyed: a peer that disconnects mid-drag
    /// would otherwise leave the button pressed in the OS forever.
    pressed_buttons: HashMap<EmulationHandle, HashSet<u32>>,
}

impl InputEmulation {
    /// Backends that show a desktop-portal permission dialog when started.
    fn is_portal_backend(backend: Backend) -> bool {
        #[cfg(libei)]
        if matches!(backend, Backend::Libei) {
            return true;
        }
        #[cfg(rdp)]
        if matches!(backend, Backend::Xdp) {
            return true;
        }
        let _ = backend;
        false
    }

    async fn with_backend(backend: Backend) -> Result<InputEmulation, EmulationCreationError> {
        let emulation: Box<dyn Emulation> = match backend {
            #[cfg(wlroots)]
            Backend::Wlroots => Box::new(wlroots::WlrootsEmulation::new()?),
            #[cfg(libei)]
            Backend::Libei => Box::new(libei::LibeiEmulation::new().await?),
            #[cfg(x11)]
            Backend::X11 => Box::new(x11::X11Emulation::new()?),
            #[cfg(rdp)]
            Backend::Xdp => Box::new(xdg_desktop_portal::DesktopPortalEmulation::new().await?),
            #[cfg(windows)]
            Backend::Windows => Box::new(windows::WindowsEmulation::new()?),
            #[cfg(target_os = "macos")]
            Backend::MacOs => Box::new(macos::MacOSEmulation::new()?),
            Backend::Dummy => Box::new(dummy::DummyEmulation::new()),
        };
        Ok(Self {
            emulation,
            backend,
            handles: HashSet::new(),
            pressed_keys: HashMap::new(),
            pressed_buttons: HashMap::new(),
        })
    }

    /// The backend that was actually selected.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Install the notifier used by backends that can detect the remote
    /// pointer being pushed past a screen edge (see [`PointerEdge`]).
    ///
    /// Backends that cannot detect it never call the notifier; the capture
    /// backend's barrier detection remains the only return path there.
    pub fn set_edge_return_notifier(&mut self, notifier: EdgeReturnNotifier) {
        self.emulation.set_edge_return_notifier(notifier);
    }

    /// Adjust the absolute-pointer sensitivity (1.0 = 100%) live.
    pub fn set_absolute_sensitivity(&mut self, gain: f64) {
        self.emulation.set_absolute_sensitivity(gain);
    }

    /// Arm edge-return tracking for a peer that entered through \`edge\`.
    pub async fn begin_remote_control(&mut self, edge: PointerEdge) {
        self.emulation.begin_remote_control(edge).await;
    }

    /// Disarm edge-return tracking (peer released / disconnected).
    pub async fn end_remote_control(&mut self) {
        self.emulation.end_remote_control().await;
    }

    pub async fn new(backend: Option<Backend>) -> Result<InputEmulation, EmulationCreationError> {
        Self::new_with_portal(backend, true).await
    }

    /// Like [\`InputEmulation::new\`], but a portal-backed backend is only
    /// selected when \`allow_portal\` is set.
    ///
    /// The service passes \`false\` when no portal consent was saved: the
    /// portal backend would open a permission dialog, so another backend (X11,
    /// wlroots, dummy) is tried instead. An explicit user enable passes \`true\`.
    pub async fn new_with_portal(
        backend: Option<Backend>,
        allow_portal: bool,
    ) -> Result<InputEmulation, EmulationCreationError> {
        if let Some(backend) = backend {
            let b = Self::with_backend(backend).await;
            if b.is_ok() {
                log::info!("using emulation backend: {backend}");
            }
            return b;
        }

        for backend in [
            #[cfg(wlroots)]
            Backend::Wlroots,
            #[cfg(libei)]
            Backend::Libei,
            #[cfg(rdp)]
            Backend::Xdp,
            #[cfg(x11)]
            Backend::X11,
            #[cfg(windows)]
            Backend::Windows,
            #[cfg(target_os = "macos")]
            Backend::MacOs,
            Backend::Dummy,
        ] {
            if !allow_portal && Self::is_portal_backend(backend) {
                log::debug!("skipping {backend}: no portal consent yet");
                continue;
            }
            // Without portal consent a dummy backend would silently swallow
            // remote input; only an explicit backend choice may select it.
            if !allow_portal && matches!(backend, Backend::Dummy) {
                continue;
            }
            match Self::with_backend(backend).await {
                Ok(b) => {
                    log::info!("using emulation backend: {backend}");
                    return Ok(b);
                }
                Err(e) if e.cancelled_by_user() => return Err(e),
                Err(e) => log::warn!("{e}"),
            }
        }

        Err(EmulationCreationError::NoAvailableBackend)
    }

    pub async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        match event {
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                let forward = match self.update_pressed_keys(handle, key, state) {
                    // No `create` for this handle (yet): never inject input that
                    // could not be released later.
                    KeyUpdate::NotTracked | KeyUpdate::DuplicateRelease => false,
                    KeyUpdate::Pressed | KeyUpdate::Released => true,
                    // The sender's OS repeats a held key by sending more
                    // key-down events. Backends that synthesize their own repeat
                    // (Windows, macOS) must not see them twice; the others only
                    // repeat if they are passed through, so the repeat rate
                    // follows the sender's OS settings.
                    KeyUpdate::DuplicatePress => !self.emulation.synthesizes_key_repeat(),
                };
                if forward {
                    self.emulation.consume(event, handle).await?;
                }
                Ok(())
            }
            Event::Pointer(PointerEvent::Button { button, state, .. }) => {
                // Track held buttons so they can be released on destroy. The
                // event itself is always forwarded.
                self.update_pressed_buttons(handle, button, state);
                self.emulation.consume(event, handle).await
            }
            _ => self.emulation.consume(event, handle).await,
        }
    }

    pub async fn create(&mut self, handle: EmulationHandle) -> bool {
        if self.handles.insert(handle) {
            self.pressed_keys.insert(handle, HashSet::new());
            self.pressed_buttons.insert(handle, HashSet::new());
            self.emulation.create(handle).await;
            true
        } else {
            false
        }
    }

    pub async fn destroy(&mut self, handle: EmulationHandle) {
        if let Err(e) = self.release_keys(handle).await {
            // Best effort: the backend is going away, but a failure here means
            // the OS may keep the last keys/buttons pressed.
            log::warn!("failed to release held input for handle {handle}: {e}");
        }
        if self.handles.remove(&handle) {
            self.pressed_keys.remove(&handle);
            self.pressed_buttons.remove(&handle);
            self.emulation.destroy(handle).await
        }
    }

    pub async fn terminate(&mut self) {
        for handle in self.handles.iter().cloned().collect::<Vec<_>>() {
            self.destroy(handle).await
        }
        self.emulation.terminate().await
    }

    /// Release every key and pointer button still held by `handle`, then reset
    /// the modifier state.
    ///
    /// Called from [`Self::destroy`] (peer disconnect, leave or watchdog
    /// timeout) and [`Self::terminate`]. Without it the receiving OS keeps the
    /// last modifier or mouse button pressed forever: a stuck Ctrl breaks every
    /// following keystroke and a stuck left button keeps selecting/dragging —
    /// the worst failure mode of a KVM.
    ///
    /// Every held input is attempted even if one release fails; a single failed
    /// event must not leave the remaining keys stuck. The first error is
    /// returned once all releases were attempted.
    pub async fn release_keys(&mut self, handle: EmulationHandle) -> Result<(), EmulationError> {
        let keys = self
            .pressed_keys
            .get_mut(&handle)
            .map(|keys| keys.drain().collect::<Vec<_>>())
            .unwrap_or_default();
        let buttons = self
            .pressed_buttons
            .get_mut(&handle)
            .map(|buttons| buttons.drain().collect::<Vec<_>>())
            .unwrap_or_default();

        let mut first_error = None;
        for key in keys {
            let event = Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key,
                state: 0,
            });
            if let Err(e) = self.emulation.consume(event, handle).await {
                first_error.get_or_insert(e);
                continue;
            }
            if let Ok(key) = input_event::scancode::Linux::try_from(key) {
                log::warn!("releasing stuck key: {key:?}");
            }
        }
        for button in buttons {
            let event = Event::Pointer(PointerEvent::Button {
                time: 0,
                button,
                state: 0,
            });
            if let Err(e) = self.emulation.consume(event, handle).await {
                first_error.get_or_insert(e);
                continue;
            }
            log::warn!("releasing stuck mouse button: {button:#x}");
        }

        // Reset the modifier state as well: a peer may have sent a Modifiers
        // event without the matching per-key events.
        let event = Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: 0,
            latched: 0,
            locked: 0,
            group: 0,
        });
        if let Err(e) = self.emulation.consume(event, handle).await {
            first_error.get_or_insert(e);
        }

        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub fn has_pressed_keys(&self, handle: EmulationHandle) -> bool {
        self.pressed_keys
            .get(&handle)
            .is_some_and(|p| !p.is_empty())
    }

    /// Update the pressed keys for the given handle and classify the event.
    fn update_pressed_keys(&mut self, handle: EmulationHandle, key: u32, state: u8) -> KeyUpdate {
        let Some(pressed_keys) = self.pressed_keys.get_mut(&handle) else {
            return KeyUpdate::NotTracked;
        };

        if state == 0 {
            // currently pressed => can release
            if pressed_keys.remove(&key) {
                KeyUpdate::Released
            } else {
                KeyUpdate::DuplicateRelease
            }
        } else {
            // currently not pressed => can press
            if pressed_keys.insert(key) {
                KeyUpdate::Pressed
            } else {
                KeyUpdate::DuplicatePress
            }
        }
    }

    /// Track (or forget) a held pointer button for the given handle.
    fn update_pressed_buttons(&mut self, handle: EmulationHandle, button: u32, state: u32) {
        let Some(pressed_buttons) = self.pressed_buttons.get_mut(&handle) else {
            return;
        };
        if state == 0 {
            pressed_buttons.remove(&button);
        } else {
            pressed_buttons.insert(button);
        }
    }
}

/// How one keyboard key event relates to the pressed-keys state of a handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyUpdate {
    /// The handle was never created: drop the event.
    NotTracked,
    /// First press of the key.
    Pressed,
    /// The key is already held: an OS auto-repeat from the sender.
    DuplicatePress,
    /// Release matching a tracked press.
    Released,
    /// Release for a key that was not tracked.
    DuplicateRelease,
}

#[async_trait]
trait Emulation: Send {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError>;
    async fn create(&mut self, handle: EmulationHandle);
    async fn destroy(&mut self, handle: EmulationHandle);
    async fn terminate(&mut self);

    /// Backends that can observe the injected pointer at a screen edge report
    /// it here so the service can hand control back to the peer.
    fn set_edge_return_notifier(&mut self, _notifier: EdgeReturnNotifier) {}

    /// Adjust the sensitivity multiplier of the absolute pointer injection
    /// (1.0 = 100%). Takes effect live; backends without absolute injection
    /// ignore it.
    fn set_absolute_sensitivity(&mut self, _gain: f64) {}

    /// A peer started driving this machine through \`edge\`.
    ///
    /// Backends that can position the pointer absolutely should warp it to a
    /// known anchor and track the injected motion from there, which makes an
    /// exact edge-return detector possible. Backends without absolute support
    /// keep the default no-op.
    async fn begin_remote_control(&mut self, _edge: PointerEdge) {}

    /// The peer no longer drives this machine; stop absolute tracking.
    async fn end_remote_control(&mut self) {}

    /// Whether this backend synthesizes key auto-repeat itself for a held key.
    ///
    /// Windows and macOS do (they have a repeat task); the Wayland/libei/X11/
    /// portal backends do not. For those, [`InputEmulation::consume`] passes the
    /// sender's repeated key-down events through instead of dropping them, so
    /// holding a key on the controlling machine keeps repeating on the receiver
    /// at the sender's OS repeat rate.
    fn synthesizes_key_repeat(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Records every event a backend is asked to inject, so the dispatch logic
    /// can be tested without touching a real input stack.
    #[derive(Default)]
    struct RecordingEmulation {
        events: Arc<Mutex<Vec<Event>>>,
        synthesize_repeat: bool,
        /// Fail every release-shaped event (key/button up, modifier reset) to
        /// exercise the release path of a peer whose backend is already dying.
        fail_on_release: bool,
    }

    #[async_trait]
    impl Emulation for RecordingEmulation {
        async fn consume(
            &mut self,
            event: Event,
            _: EmulationHandle,
        ) -> Result<(), EmulationError> {
            self.events.lock().expect("recording lock").push(event);
            let is_release = matches!(
                event,
                Event::Keyboard(KeyboardEvent::Key { state: 0, .. })
                    | Event::Keyboard(KeyboardEvent::Modifiers { .. })
                    | Event::Pointer(PointerEvent::Button { state: 0, .. })
            );
            if self.fail_on_release && is_release {
                return Err(EmulationError::EndOfStream);
            }
            Ok(())
        }
        async fn create(&mut self, _: EmulationHandle) {}
        async fn destroy(&mut self, _: EmulationHandle) {}
        async fn terminate(&mut self) {}
        fn synthesizes_key_repeat(&self) -> bool {
            self.synthesize_repeat
        }
    }

    fn recording(
        synthesize_repeat: bool,
        fail_on_release: bool,
    ) -> (InputEmulation, Arc<Mutex<Vec<Event>>>) {
        let events: Arc<Mutex<Vec<Event>>> = Default::default();
        let emulation = InputEmulation {
            emulation: Box::new(RecordingEmulation {
                events: events.clone(),
                synthesize_repeat,
                fail_on_release,
            }),
            backend: Backend::Dummy,
            handles: HashSet::new(),
            pressed_keys: HashMap::new(),
            pressed_buttons: HashMap::new(),
        };
        (emulation, events)
    }

    fn recorded(events: &Arc<Mutex<Vec<Event>>>) -> Vec<Event> {
        events.lock().expect("recording lock").clone()
    }

    fn key(key: u32, state: u8) -> Event {
        Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key,
            state,
        })
    }

    fn button(button: u32, state: u32) -> Event {
        Event::Pointer(PointerEvent::Button {
            time: 0,
            button,
            state,
        })
    }

    fn modifiers_zero() -> Event {
        Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: 0,
            latched: 0,
            locked: 0,
            group: 0,
        })
    }

    #[tokio::test]
    async fn dummy_backend_is_selectable_and_reports_itself() {
        let emulation = InputEmulation::new(Some(Backend::Dummy))
            .await
            .expect("dummy emulation");
        assert_eq!(emulation.backend(), Backend::Dummy);
    }

    #[tokio::test]
    async fn key_events_are_classified_by_the_pressed_keys_state() {
        let (mut emulation, _events) = recording(false, false);
        assert!(emulation.create(0).await);

        assert_eq!(
            emulation.update_pressed_keys(0, 30, 1),
            KeyUpdate::Pressed,
            "first press"
        );
        assert_eq!(
            emulation.update_pressed_keys(0, 30, 1),
            KeyUpdate::DuplicatePress,
            "auto-repeat from the sender"
        );
        assert_eq!(emulation.pressed_keys.get(&0).map(HashSet::len), Some(1));

        assert_eq!(
            emulation.update_pressed_keys(0, 30, 0),
            KeyUpdate::Released,
            "first release"
        );
        assert_eq!(
            emulation.update_pressed_keys(0, 30, 0),
            KeyUpdate::DuplicateRelease,
            "duplicate release"
        );
        assert_eq!(emulation.pressed_keys.get(&0).map(HashSet::len), Some(0));

        // events for an unknown handle are ignored
        assert_eq!(
            emulation.update_pressed_keys(99, 30, 1),
            KeyUpdate::NotTracked
        );
    }

    /// Regression for the Windows -> Linux direction: the sender repeats a held
    /// key by sending more key-down events, and backends without a repeat task
    /// must receive them instead of having them filtered out.
    #[tokio::test]
    async fn backends_without_native_repeat_receive_the_senders_repeats() {
        let (mut emulation, events) = recording(false, false);
        assert!(emulation.create(0).await);

        for _ in 0..3 {
            emulation.consume(key(30, 1), 0).await.expect("consume");
        }
        assert_eq!(
            recorded(&events),
            vec![key(30, 1), key(30, 1), key(30, 1)],
            "the repeat presses must reach the backend"
        );

        emulation.consume(key(30, 0), 0).await.expect("consume");
        emulation.consume(key(30, 0), 0).await.expect("consume");
        assert_eq!(
            recorded(&events).len(),
            4,
            "a duplicate release must still be filtered"
        );
    }

    /// Windows and macOS synthesize their own repeat: the sender's repeated
    /// presses must not be injected a second time.
    #[tokio::test]
    async fn backends_with_native_repeat_do_not_receive_duplicate_presses() {
        let (mut emulation, events) = recording(true, false);
        assert!(emulation.create(0).await);

        for _ in 0..3 {
            emulation.consume(key(30, 1), 0).await.expect("consume");
        }
        emulation.consume(key(30, 0), 0).await.expect("consume");
        assert_eq!(recorded(&events), vec![key(30, 1), key(30, 0)]);
    }

    #[tokio::test]
    async fn events_for_unknown_handles_are_dropped() {
        let (mut emulation, events) = recording(false, false);
        emulation.consume(key(30, 1), 99).await.expect("consume");
        assert!(recorded(&events).is_empty());
    }

    /// A peer disconnecting (or the watchdog timing out) while keys and a mouse
    /// button are held must release all of them: a stuck Ctrl or a stuck left
    /// button is the worst failure mode of a KVM.
    #[tokio::test]
    async fn destroy_releases_held_keys_and_buttons() {
        let (mut emulation, events) = recording(false, false);
        assert!(emulation.create(1).await);
        emulation.consume(key(29, 1), 1).await.expect("consume");
        emulation.consume(key(30, 1), 1).await.expect("consume");
        emulation
            .consume(button(input_event::BTN_LEFT, 1), 1)
            .await
            .expect("consume");

        emulation.destroy(1).await;

        let events = recorded(&events);
        for expected in [
            key(29, 0),
            key(30, 0),
            button(input_event::BTN_LEFT, 0),
            modifiers_zero(),
        ] {
            assert!(
                events.contains(&expected),
                "missing release event: {expected:?} (got {events:?})"
            );
        }
        assert!(!emulation.pressed_buttons.contains_key(&1));
        assert!(!emulation.pressed_keys.contains_key(&1));
    }

    /// One failed release must not leave the remaining keys/buttons stuck: all
    /// releases are attempted and the first error is returned afterwards.
    #[tokio::test]
    async fn release_attempts_every_held_input_when_one_fails() {
        let (mut emulation, events) = recording(false, true);
        assert!(emulation.create(2).await);
        emulation.consume(key(29, 1), 2).await.expect("tracked");
        emulation.consume(key(30, 1), 2).await.expect("tracked");
        emulation
            .consume(button(input_event::BTN_RIGHT, 1), 2)
            .await
            .expect("tracked");

        let result = emulation.release_keys(2).await;
        assert!(result.is_err(), "the backend error must be reported");

        let events = recorded(&events);
        for expected in [
            key(29, 0),
            key(30, 0),
            button(input_event::BTN_RIGHT, 0),
            modifiers_zero(),
        ] {
            assert!(
                events.contains(&expected),
                "missing release attempt: {expected:?} (got {events:?})"
            );
        }
    }

    #[tokio::test]
    async fn destroy_drops_pressed_key_and_button_state() {
        let (mut emulation, _events) = recording(false, false);
        assert!(emulation.create(1).await);
        assert!(!emulation.create(1).await, "duplicate create is a no-op");
        emulation.consume(key(42, 1), 1).await.expect("consume");
        emulation
            .consume(button(input_event::BTN_MIDDLE, 1), 1)
            .await
            .expect("consume");
        assert!(emulation.has_pressed_keys(1));

        emulation.destroy(1).await;
        assert!(!emulation.handles.contains(&1));
        assert!(!emulation.pressed_keys.contains_key(&1));
        assert!(!emulation.pressed_buttons.contains_key(&1));
    }
}
