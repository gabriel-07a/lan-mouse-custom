use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Display,
    mem::swap,
    task::{Poll, ready},
};

use async_trait::async_trait;
use futures::StreamExt;
use futures_core::Stream;

use input_event::{Event, KeyboardEvent, scancode};

pub use error::{CaptureCreationError, CaptureError, InputCaptureError};

pub mod error;

#[cfg(libei)]
mod libei;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(layer_shell)]
mod layer_shell;

#[cfg(not(windows))]
#[path = "windows/display_util.rs"]
mod display_util;
#[cfg(windows)]
mod windows;

#[cfg(x11)]
mod x11;

/// fallback input capture (does not produce events)
mod dummy;

pub type CaptureHandle = u64;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum CaptureEvent {
    /// capture on this capture handle is now active
    Begin,
    /// input event coming from capture handle
    Input(Event),
}

impl Display for CaptureEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureEvent::Begin => write!(f, "begin capture"),
            CaptureEvent::Input(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Self::Right,
            Position::Right => Self::Left,
            Position::Top => Self::Bottom,
            Position::Bottom => Self::Top,
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(libei)]
    InputCapturePortal,
    #[cfg(layer_shell)]
    LayerShell,
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
            #[cfg(libei)]
            Backend::InputCapturePortal => write!(f, "input-capture-portal"),
            #[cfg(layer_shell)]
            Backend::LayerShell => write!(f, "layer-shell"),
            #[cfg(x11)]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "MacOS"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

pub struct InputCapture {
    /// capture backend
    capture: Box<dyn Capture>,
    /// backend that was actually selected
    backend: Backend,
    /// keys pressed by active capture
    pressed_keys: HashSet<scancode::Linux>,
    /// map from position to ids
    position_map: HashMap<Position, Vec<CaptureHandle>>,
    /// map from id to position
    id_map: HashMap<CaptureHandle, Position>,
    /// pending events
    pending: VecDeque<(CaptureHandle, CaptureEvent)>,
}

impl InputCapture {
    /// create a new client with the given id
    ///
    /// Idempotent: a handle that already exists is left untouched. The backend
    /// is only asked to create a capture for a position that has no client
    /// yet; if that fails the bookkeeping is rolled back so a retry (or the
    /// matching `destroy`) cannot hit an inconsistent state.
    pub async fn create(&mut self, id: CaptureHandle, pos: Position) -> Result<(), CaptureError> {
        if let Some(existing) = self.id_map.get(&id) {
            log::warn!("capture {id} already exists @ {existing} (requested @ {pos}), ignoring");
            return Ok(());
        }

        self.id_map.insert(id, pos);

        if let Some(v) = self.position_map.get_mut(&pos) {
            v.push(id);
            Ok(())
        } else {
            self.position_map.insert(pos, vec![id]);
            if let Err(e) = self.capture.create(pos).await {
                // Roll back: otherwise the position stays registered (later
                // creates for the same edge would skip the backend) and the
                // id maps to a capture the backend never created.
                self.position_map.remove(&pos);
                self.id_map.remove(&id);
                return Err(e);
            }
            Ok(())
        }
    }

    /// destroy the client with the given id, if it exists
    ///
    /// Tolerant of unknown handles: the service may request a teardown for a
    /// capture that was never created (or already destroyed), and panicking
    /// here would abort the process in release builds (panic = "abort").
    pub async fn destroy(&mut self, id: CaptureHandle) -> Result<(), CaptureError> {
        let Some(pos) = self.id_map.remove(&id) else {
            log::warn!("destroy of unknown capture {id} ignored");
            return Ok(());
        };

        log::debug!("destroying capture {id} @ {pos}");
        let Some(remaining) = self.position_map.get_mut(&pos) else {
            log::warn!("capture {id} @ {pos} without a registered position, ignoring");
            return Ok(());
        };
        remaining.retain(|&i| i != id);

        log::debug!("remaining ids @ {pos}: {remaining:?}");
        if remaining.is_empty() {
            log::debug!("destroying capture @ {pos} - no remaining ids");
            self.position_map.remove(&pos);
            self.capture.destroy(pos).await?;
        }
        Ok(())
    }

    /// release mouse
    pub async fn release(&mut self) -> Result<(), CaptureError> {
        self.pressed_keys.clear();
        self.capture.release().await
    }

    /// Tell the backend which edge a remote peer entered through and is
    /// currently driving this machine from (`None` = no peer is driving us).
    pub fn set_remote_peer(&mut self, peer: Option<Position>) {
        self.capture.set_remote_peer(peer);
    }

    /// Drain and return every key the capture has forwarded as
    /// down-but-not-up. The caller is expected to synthesize key-up
    /// events to the remote peer for each — otherwise the peer
    /// retains phantom-held keys after capture is released. The
    /// canonical case is the release-bind chord
    /// (Ctrl+Shift+Alt+Meta): the down events were sent while
    /// capture was active, but the matching up events arrive after
    /// the local tap has flipped to passthrough and never reach
    /// the peer.
    pub fn take_pressed_keys(&mut self) -> HashSet<scancode::Linux> {
        std::mem::take(&mut self.pressed_keys)
    }

    /// destroy the input capture
    pub async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.capture.terminate().await
    }

    /// creates a new [`InputCapture`]
    pub async fn new(backend: Option<Backend>) -> Result<Self, CaptureCreationError> {
        let (backend, capture) = create(backend).await?;
        Ok(Self {
            capture,
            backend,
            id_map: Default::default(),
            pending: Default::default(),
            position_map: Default::default(),
            pressed_keys: HashSet::new(),
        })
    }

    /// the backend that was actually selected
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// check whether the given keys are pressed.
    /// An empty release bind must never release the capture.
    pub fn keys_pressed(&self, keys: &[scancode::Linux]) -> bool {
        keys_pressed_in(&self.pressed_keys, keys)
    }

    fn update_pressed_keys(&mut self, key: u32, state: u8) {
        if let Ok(scancode) = scancode::Linux::try_from(key) {
            log::debug!("key: {key}, state: {state}, scancode: {scancode:?}");
            // 0 = released, everything else (press, evdev autorepeat) means
            // the key is down. Treating a repeat (2) as a release would drop
            // a held modifier from `pressed_keys` and the phantom-key release
            // on a hand-off would then miss it.
            if state == 0 {
                self.pressed_keys.remove(&scancode);
            } else {
                self.pressed_keys.insert(scancode);
            }
        }
    }
}

fn keys_pressed_in(pressed: &HashSet<scancode::Linux>, keys: &[scancode::Linux]) -> bool {
    !keys.is_empty() && keys.iter().all(|key| pressed.contains(key))
}

impl Stream for InputCapture {
    type Item = Result<(CaptureHandle, CaptureEvent), CaptureError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(e) = self.pending.pop_front() {
                return Poll::Ready(Some(Ok(e)));
            }

            // ready
            let event = ready!(self.capture.poll_next_unpin(cx));

            // stream closed
            let event = match event {
                Some(e) => e,
                None => return Poll::Ready(None),
            };

            // error occurred
            let (pos, event) = match event {
                Ok(e) => e,
                Err(e) => return Poll::Ready(Some(Err(e))),
            };

            // handle key presses
            if let CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key { key, state, .. })) =
                event
            {
                self.update_pressed_keys(key, state);
            }

            let len = self
                .position_map
                .get(&pos)
                .map(|ids| ids.len())
                .unwrap_or(0);

            match len {
                // No capture registered for this position (for example an
                // event that raced a `destroy`). Drop it and keep polling
                // instead of returning `Pending` without registering a
                // waker, which would stall the stream until an unrelated
                // wakeup.
                0 => continue,
                1 => {
                    return Poll::Ready(Some(Ok((
                        self.position_map.get(&pos).expect("no id")[0],
                        event,
                    ))));
                }
                _ => {
                    let mut position_map = HashMap::new();
                    swap(&mut self.position_map, &mut position_map);
                    {
                        for &id in position_map.get(&pos).expect("position") {
                            self.pending.push_back((id, event));
                        }
                    }
                    swap(&mut self.position_map, &mut position_map);

                    return Poll::Ready(Some(Ok(self.pending.pop_front().expect("event"))));
                }
            }
        }
    }
}

#[async_trait]
trait Capture: Stream<Item = Result<(Position, CaptureEvent), CaptureError>> + Unpin {
    /// create a new client with the given id
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError>;

    /// destroy the client with the given id, if it exists
    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError>;

    /// release mouse
    async fn release(&mut self) -> Result<(), CaptureError>;

    /// destroy the input capture
    async fn terminate(&mut self) -> Result<(), CaptureError>;

    /// Tell the backend which edge a remote peer entered through and is
    /// currently driving this machine from (`None` = no peer is driving us).
    ///
    /// Backends whose local edge detection can observe injected input (Windows
    /// raw input / low level hooks) must not start a local capture from that
    /// input, otherwise the two machines hand control back and forth at the
    /// shared border. The exception is the edge the peer entered through: a
    /// crossing there is the return hand-off and must still be reported.
    fn set_remote_peer(&mut self, _peer: Option<Position>) {}
}

async fn create_backend(
    backend: Backend,
) -> Result<
    Box<dyn Capture<Item = Result<(Position, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    match backend {
        #[cfg(libei)]
        Backend::InputCapturePortal => Ok(Box::new(libei::LibeiInputCapture::new().await?)),
        #[cfg(layer_shell)]
        Backend::LayerShell => Ok(Box::new(layer_shell::LayerShellInputCapture::new()?)),
        #[cfg(x11)]
        Backend::X11 => Ok(Box::new(x11::X11InputCapture::new()?)),
        #[cfg(windows)]
        Backend::Windows => Ok(Box::new(windows::WindowsInputCapture::new()?)),
        #[cfg(target_os = "macos")]
        Backend::MacOs => Ok(Box::new(macos::MacOSInputCapture::new().await?)),
        Backend::Dummy => Ok(Box::new(dummy::DummyInputCapture::new())),
    }
}

async fn create(
    backend: Option<Backend>,
) -> Result<
    (
        Backend,
        Box<dyn Capture<Item = Result<(Position, CaptureEvent), CaptureError>>>,
    ),
    CaptureCreationError,
> {
    if let Some(backend) = backend {
        let b = create_backend(backend).await;
        if b.is_ok() {
            log::info!("using capture backend: {backend}");
        }
        return b.map(|b| (backend, b));
    }

    for backend in [
        #[cfg(libei)]
        Backend::InputCapturePortal,
        #[cfg(layer_shell)]
        Backend::LayerShell,
        #[cfg(x11)]
        Backend::X11,
        #[cfg(windows)]
        Backend::Windows,
        #[cfg(target_os = "macos")]
        Backend::MacOs,
    ] {
        match create_backend(backend).await {
            Ok(b) => {
                log::info!("using capture backend: {backend}");
                return Ok((backend, b));
            }
            Err(e) if e.cancelled_by_user() => return Err(e),
            Err(e) => log::warn!("{backend} input capture backend unavailable: {e}"),
        }
    }
    Err(CaptureCreationError::NoAvailableBackend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::Context;

    /// Observations recorded by [`MockCapture`], shared with the test so the
    /// backend calls can be asserted after it moved into the InputCapture.
    #[derive(Default)]
    struct MockLog {
        created: Vec<Position>,
        destroyed: Vec<Position>,
        /// fail the next create call
        fail_next_create: bool,
    }

    /// Minimal backend used to exercise the state bookkeeping in
    /// InputCapture without touching a real input device.
    struct MockCapture {
        events: VecDeque<(Position, CaptureEvent)>,
        log: Arc<Mutex<MockLog>>,
    }

    impl MockCapture {
        fn new(log: Arc<Mutex<MockLog>>) -> Self {
            Self {
                events: VecDeque::new(),
                log,
            }
        }
    }

    #[async_trait]
    impl Capture for MockCapture {
        async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
            let mut log = self.log.lock().expect("mock log");
            if log.fail_next_create {
                log.fail_next_create = false;
                return Err(CaptureError::EndOfStream);
            }
            log.created.push(pos);
            Ok(())
        }

        async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
            self.log.lock().expect("mock log").destroyed.push(pos);
            Ok(())
        }

        async fn release(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }

        async fn terminate(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }
    }

    impl Stream for MockCapture {
        type Item = Result<(Position, CaptureEvent), CaptureError>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            match self.events.pop_front() {
                Some(event) => Poll::Ready(Some(Ok(event))),
                None => Poll::Pending,
            }
        }
    }

    fn input_capture_with(mock: MockCapture) -> InputCapture {
        InputCapture {
            capture: Box::new(mock),
            backend: Backend::Dummy,
            pressed_keys: HashSet::new(),
            position_map: HashMap::new(),
            id_map: HashMap::new(),
            pending: VecDeque::new(),
        }
    }

    #[test]
    fn position_opposite_is_involutive() {
        for pos in [
            Position::Left,
            Position::Right,
            Position::Top,
            Position::Bottom,
        ] {
            assert_eq!(pos.opposite().opposite(), pos);
            assert_ne!(pos.opposite(), pos);
        }
    }

    #[test]
    fn empty_release_bind_never_matches() {
        let pressed = HashSet::from([scancode::Linux::KeyA]);
        assert!(!keys_pressed_in(&pressed, &[]));
        assert!(keys_pressed_in(&pressed, &[scancode::Linux::KeyA]));
        assert!(!keys_pressed_in(
            &pressed,
            &[scancode::Linux::KeyA, scancode::Linux::KeyB]
        ));
        assert!(!keys_pressed_in(&HashSet::new(), &[scancode::Linux::KeyA]));
    }

    #[test]
    fn position_display_is_lowercase() {
        assert_eq!(Position::Left.to_string(), "left");
        assert_eq!(Position::Bottom.to_string(), "bottom");
    }

    fn new_mock() -> (Arc<Mutex<MockLog>>, InputCapture) {
        let log = Arc::new(Mutex::new(MockLog::default()));
        let capture = input_capture_with(MockCapture::new(log.clone()));
        (log, capture)
    }

    #[tokio::test]
    async fn a_failed_backend_create_rolls_the_bookkeeping_back() {
        let (log, mut capture) = new_mock();
        log.lock().expect("mock log").fail_next_create = true;

        assert!(capture.create(7, Position::Left).await.is_err());
        assert!(capture.id_map.is_empty(), "id must not survive the failure");
        assert!(
            capture.position_map.is_empty(),
            "position must not stay registered (a retry would skip the backend)"
        );

        // A retry must reach the backend again instead of silently registering.
        capture.create(7, Position::Left).await.unwrap();
        assert_eq!(capture.id_map.get(&7), Some(&Position::Left));
        assert_eq!(log.lock().expect("mock log").created, vec![Position::Left]);
    }

    #[tokio::test]
    async fn a_duplicate_handle_is_ignored_instead_of_panicking() {
        let (log, mut capture) = new_mock();
        capture.create(7, Position::Left).await.unwrap();
        // The service may re-create a capture for a handle whose destroy is
        // still queued; aborting the process here is not acceptable.
        capture.create(7, Position::Right).await.unwrap();

        assert_eq!(capture.id_map.get(&7), Some(&Position::Left));
        assert_eq!(capture.position_map.keys().count(), 1);
        assert!(!capture.position_map.contains_key(&Position::Right));
        assert_eq!(log.lock().expect("mock log").created, vec![Position::Left]);
    }

    #[tokio::test]
    async fn destroying_an_unknown_handle_is_ignored_instead_of_panicking() {
        let (log, mut capture) = new_mock();
        capture.destroy(42).await.unwrap();
        assert!(log.lock().expect("mock log").destroyed.is_empty());
    }

    #[tokio::test]
    async fn destroying_the_last_handle_at_a_position_tears_the_backend_down() {
        let (log, mut capture) = new_mock();
        capture.create(1, Position::Top).await.unwrap();
        capture.create(2, Position::Top).await.unwrap();

        capture.destroy(1).await.unwrap();
        assert!(
            log.lock().expect("mock log").destroyed.is_empty(),
            "another handle is still registered at the position"
        );

        capture.destroy(2).await.unwrap();
        assert_eq!(log.lock().expect("mock log").destroyed, vec![Position::Top]);
        assert!(capture.position_map.is_empty());
    }

    #[test]
    fn autorepeat_keeps_a_key_in_the_pressed_set() {
        let (_log, mut capture) = new_mock();
        let key = scancode::Linux::KeyA as u32;

        capture.update_pressed_keys(key, 1);
        assert!(capture.keys_pressed(&[scancode::Linux::KeyA]));
        // evdev autorepeat: the key is still held, only the release removes it.
        capture.update_pressed_keys(key, 2);
        assert!(capture.keys_pressed(&[scancode::Linux::KeyA]));
        capture.update_pressed_keys(key, 0);
        assert!(!capture.keys_pressed(&[scancode::Linux::KeyA]));
    }

    #[test]
    fn press_edges_are_removed_from_the_pressed_set_on_release() {
        let (_log, mut capture) = new_mock();
        capture.update_pressed_keys(scancode::Linux::KeyLeftCtrl as u32, 1);
        capture.update_pressed_keys(scancode::Linux::KeyLeftShift as u32, 1);
        assert!(
            capture.keys_pressed(&[scancode::Linux::KeyLeftCtrl, scancode::Linux::KeyLeftShift,])
        );
        let drained = capture.take_pressed_keys();
        assert_eq!(drained.len(), 2);
        assert!(!capture.keys_pressed(&[scancode::Linux::KeyLeftCtrl]));
    }
}
