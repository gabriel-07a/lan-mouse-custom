//! One virtual PC screen and the two fake backends that share it.
//!
//! A real machine has a single cursor that two lan-mouse backends touch:
//! the capture backend watches the local mouse hit a screen edge, and the
//! emulation backend moves the cursor while a peer drives the machine. Both
//! fakes here hold the same [`SharedScreen`], so they see the same cursor,
//! exactly like on a real desk.
//!
//! The behaviour is the *baseline contract* of the backends (no OS quirks):
//!
//! - capture: local motion moves the cursor; pushing it past an edge that has
//!   a barrier starts a capture (`BeginAt(along)`), after which local motion is
//!   forwarded as `Input` and the local cursor stays put until `release`;
//! - emulation: injected motion moves the cursor and is clamped to the
//!   screen; pushing past the edge the peer entered through reports an edge
//!   return (throttled like the libei/Windows trackers).

use std::{
    collections::HashSet,
    fmt,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::Stream;
use input_capture::{CaptureError, CaptureEvent, Position};
use input_emulation::{EdgeReturnNotifier, EmulationError, EmulationHandle, PointerEdge};
use input_event::{Event, PointerEvent};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Same throttle as `input-emulation/src/absolute.rs`: a peer that keeps
/// pushing past the edge re-requests the return at most this often.
const EDGE_RETURN_THROTTLE: Duration = Duration::from_millis(250);
/// Same cap as the real trackers: requests per contiguous edge contact.
const EDGE_RETURN_MAX_ATTEMPTS: u8 = 3;

/// OS quirks of a virtual PC, on top of the baseline backend contract.
///
/// The numbers come from real traces (see `docs/analysis/handoff-bidirectional.md`
/// and the models in `tests/qa_handoff_bidirectional.rs`). Each field is
/// what the OS backend *delivers to lan-mouse*, so the service's own guards
/// are what is being tested.
#[derive(Clone, Debug)]
pub(crate) struct Personality {
    pub(crate) label: &'static str,
    /// Source side: deltas the compositor emits right after a capture starts,
    /// relative to the crossing direction (negative = back towards this
    /// screen). GNOME/libei warps the pointer back from the barrier.
    pub(crate) activation_backswing: Vec<f64>,
    /// Source side: one more backswing delivered right after the first user
    /// motion of a capture (GNOME: ~17 px).
    pub(crate) post_forward_backswing: Option<f64>,
    /// Receiver side: while a peer drives this screen, injected motion that
    /// comes within this many px of an edge with a barrier makes the capture
    /// backend report a (jittery) `Begin` there (GNOME capture portal).
    pub(crate) begin_jitter_px: Option<f64>,
    /// Receiver side: injected motion pushed past the edge the peer entered
    /// through is also seen by the capture backend, which reports a `Begin`
    /// there besides the emulation's edge return (Windows hooks see injected
    /// input).
    pub(crate) injected_crossing_begins_capture: bool,
}

impl Personality {
    /// Baseline contract only, no quirks.
    pub(crate) fn baseline() -> Self {
        Self {
            label: "baseline",
            activation_backswing: Vec::new(),
            post_forward_backswing: None,
            begin_jitter_px: None,
            injected_crossing_begins_capture: false,
        }
    }

    /// Ubuntu/GNOME Wayland (libei capture portal + RemoteDesktop emulation).
    pub(crate) fn gnome() -> Self {
        Self {
            label: "gnome",
            // trace: -2, -5 before the crossing motion, -17 after it
            activation_backswing: vec![-2.0, -5.0],
            post_forward_backswing: Some(-17.0),
            begin_jitter_px: Some(2.0),
            injected_crossing_begins_capture: false,
        }
    }

    /// Windows 10/11 (low-level hooks + SendInput emulation).
    pub(crate) fn windows() -> Self {
        Self {
            label: "windows",
            activation_backswing: Vec::new(),
            post_forward_backswing: None,
            begin_jitter_px: None,
            injected_crossing_begins_capture: true,
        }
    }
}

impl Default for Personality {
    fn default() -> Self {
        Self::baseline()
    }
}

/// Minimum time between two jittery `Begin`s of [`Personality::begin_jitter_px`].
const BEGIN_JITTER_INTERVAL: Duration = Duration::from_millis(25);

/// Unit vector pointing out of the screen through `edge`.
fn outward(edge: Position) -> (f64, f64) {
    match edge {
        Position::Left => (-1.0, 0.0),
        Position::Right => (1.0, 0.0),
        Position::Top => (0.0, -1.0),
        Position::Bottom => (0.0, 1.0),
    }
}

/// Something observable that happened on a virtual screen.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ScreenEvent {
    /// the service created a capture barrier at this edge
    BarrierCreated(Position),
    /// the service removed the capture barrier at this edge
    BarrierDestroyed(Position),
    /// the local mouse crossed a barrier: input now goes to a peer
    CaptureBegin { edge: Position, along: f32 },
    /// the capture was released (`at` = return coordinate, if any)
    CaptureReleased { at: Option<(Position, f32)> },
    /// a peer started driving this screen through `edge`
    RemoteBegin {
        edge: PointerEdge,
        along: Option<f32>,
    },
    /// the peer stopped driving this screen
    RemoteEnd,
    /// the injected pointer was pushed past the entry edge (return request)
    EdgeReturn { edge: PointerEdge, along: f32 },
    /// a [`Personality`] quirk fired
    Quirk(&'static str),
}

/// A [`ScreenEvent`] with the time it happened.
#[derive(Clone, Debug)]
pub(crate) struct Logged {
    pub(crate) at: Instant,
    pub(crate) event: ScreenEvent,
}

/// State of one virtual PC.
pub(crate) struct ScreenState {
    pub(crate) name: &'static str,
    pub(crate) width: f64,
    pub(crate) height: f64,
    /// cursor position in pixels, `0..width` x `0..height`
    pub(crate) cursor: (f64, f64),
    /// edges with a capture barrier (created by the service)
    pub(crate) barriers: HashSet<Position>,
    /// a local capture is active through this edge: local motion goes to a peer
    pub(crate) captured: Option<Position>,
    /// a peer drives this screen; it entered through this edge
    pub(crate) remote_edge: Option<PointerEdge>,
    /// what the service told the capture backend through `set_remote_peer`
    pub(crate) remote_peer_hint: Option<Position>,
    /// injected motion events received from peers
    pub(crate) injected_motions: usize,
    /// injected non-motion events (buttons, keys, scroll)
    pub(crate) injected_other: Vec<Event>,
    /// Caps Lock / Num Lock of this PC ([`lan_mouse_proto::lock_bits`]), as
    /// the OS keeps it: toggled by a local lock key and by an injected one
    pub(crate) locks: u8,
    /// lock keys injected by the emulation backend (lock sync)
    pub(crate) injected_lock_taps: usize,
    pub(crate) log: Vec<Logged>,
    pub(crate) personality: Personality,
    /// backswing still owed after the first user motion of this capture
    pending_backswing: Option<f64>,
    last_jitter: Option<Instant>,
    capture_tx: Option<UnboundedSender<(Position, CaptureEvent)>>,
    edge_return: Option<EdgeReturnNotifier>,
    edge_contact: Option<EdgeContact>,
}

#[derive(Clone, Copy)]
struct EdgeContact {
    last_sent: Instant,
    attempts: u8,
}

pub(crate) type SharedScreen = Arc<Mutex<ScreenState>>;

impl ScreenState {
    pub(crate) fn new(name: &'static str, width: u32, height: u32) -> Self {
        let (width, height) = (f64::from(width), f64::from(height));
        Self {
            name,
            width,
            height,
            cursor: (width / 2.0, height / 2.0),
            barriers: HashSet::new(),
            captured: None,
            remote_edge: None,
            remote_peer_hint: None,
            injected_motions: 0,
            injected_other: Vec::new(),
            locks: 0,
            injected_lock_taps: 0,
            log: Vec::new(),
            personality: Personality::baseline(),
            pending_backswing: None,
            last_jitter: None,
            capture_tx: None,
            edge_return: None,
            edge_contact: None,
        }
    }

    pub(crate) fn shared(self) -> SharedScreen {
        Arc::new(Mutex::new(self))
    }

    fn record(&mut self, event: ScreenEvent) {
        log::debug!("desk-sim[{}]: {event:?}", self.name);
        self.log.push(Logged {
            at: Instant::now(),
            event,
        });
    }

    fn max_x(&self) -> f64 {
        self.width - 1.0
    }

    fn max_y(&self) -> f64 {
        self.height - 1.0
    }

    /// Normalised coordinate of the cursor along `edge` (0.0 = top/left).
    fn along(&self, vertical_edge: bool) -> f32 {
        let along = if vertical_edge {
            self.cursor.1 / self.max_y()
        } else {
            self.cursor.0 / self.max_x()
        };
        along.clamp(0.0, 1.0) as f32
    }

    /// Put the cursor on `edge` at the normalised coordinate `along`.
    fn place_on_edge(&mut self, edge: Position, along: f32) {
        let along = f64::from(along.clamp(0.0, 1.0));
        self.cursor = match edge {
            Position::Left => (0.0, along * self.max_y()),
            Position::Right => (self.max_x(), along * self.max_y()),
            Position::Top => (along * self.max_x(), 0.0),
            Position::Bottom => (along * self.max_x(), self.max_y()),
        };
    }

    /// The user moved the mouse that is physically attached to this PC.
    pub(crate) fn local_motion(&mut self, dx: f64, dy: f64) {
        if let Some(edge) = self.captured {
            // captured: the OS cursor does not move, the delta goes to the peer
            self.send_capture(edge, CaptureEvent::Input(motion(dx, dy)));
            if let Some(backswing) = self.pending_backswing.take() {
                self.record(ScreenEvent::Quirk("post-forward backswing"));
                self.send_backswing(edge, backswing);
            }
            return;
        }
        let (x, y) = (self.cursor.0 + dx, self.cursor.1 + dy);
        let crossed = [
            (Position::Left, x < 0.0),
            (Position::Right, x > self.max_x()),
            (Position::Top, y < 0.0),
            (Position::Bottom, y > self.max_y()),
        ]
        .into_iter()
        .find(|&(edge, past)| past && self.barriers.contains(&edge))
        .map(|(edge, _)| edge);
        self.cursor = (x.clamp(0.0, self.max_x()), y.clamp(0.0, self.max_y()));
        if let Some(edge) = crossed {
            self.begin_capture(edge);
            let backswing = self.personality.activation_backswing.clone();
            if !backswing.is_empty() {
                self.record(ScreenEvent::Quirk("activation backswing"));
            }
            for delta in backswing {
                self.send_backswing(edge, delta);
            }
            self.pending_backswing = self.personality.post_forward_backswing;
        }
    }

    /// The capture backend grabs the pointer at `edge` and reports `BeginAt`.
    fn begin_capture(&mut self, edge: Position) {
        let along = self.along(matches!(edge, Position::Left | Position::Right));
        self.captured = Some(edge);
        self.record(ScreenEvent::CaptureBegin { edge, along });
        self.send_capture(edge, CaptureEvent::BeginAt(along));
    }

    /// The user presses and releases `key` on this PC's keyboard. While a
    /// local capture is active the key goes to the peer and the local OS
    /// never sees it (the real grab); otherwise a lock key toggles the local
    /// lock.
    pub(crate) fn key_tap(&mut self, key: u32) {
        if let Some(edge) = self.captured {
            for state in [1, 0] {
                self.send_capture(
                    edge,
                    CaptureEvent::Input(Event::Keyboard(input_event::KeyboardEvent::Key {
                        time: 0,
                        key,
                        state,
                    })),
                );
            }
            return;
        }
        self.toggle_lock(key);
    }

    fn toggle_lock(&mut self, key: u32) {
        match key {
            crate::lock_sync::KEY_CAPSLOCK => self.locks ^= lan_mouse_proto::lock_bits::CAPS,
            crate::lock_sync::KEY_NUMLOCK => self.locks ^= lan_mouse_proto::lock_bits::NUM,
            _ => {}
        }
    }

    /// Forward a delta of `amount` px along the crossing direction of `edge`
    /// (negative = back towards this screen).
    fn send_backswing(&mut self, edge: Position, amount: f64) {
        let (ox, oy) = outward(edge);
        self.send_capture(edge, CaptureEvent::Input(motion(ox * amount, oy * amount)));
    }

    fn send_capture(&mut self, edge: Position, event: CaptureEvent) {
        let delivered = self
            .capture_tx
            .as_ref()
            .is_some_and(|tx| tx.send((edge, event)).is_ok());
        if !delivered {
            log::warn!(
                "desk-sim[{}]: capture event {event:?} dropped (no backend)",
                self.name
            );
        }
    }

    fn release(&mut self, at: Option<(Position, f32)>) {
        self.captured = None;
        self.pending_backswing = None;
        if let Some((edge, along)) = at {
            self.place_on_edge(edge, along);
        }
        self.record(ScreenEvent::CaptureReleased { at });
    }

    fn begin_remote(&mut self, edge: PointerEdge, along: Option<f32>) {
        self.remote_edge = Some(edge);
        self.edge_contact = None;
        // like the libei/Windows absolute trackers: warp to the crossing point
        // (edge centre when the peer sent no coordinate)
        self.place_on_edge(to_position(edge), along.unwrap_or(0.5));
        self.record(ScreenEvent::RemoteBegin { edge, along });
    }

    fn end_remote(&mut self) {
        if self.remote_edge.take().is_some() {
            self.record(ScreenEvent::RemoteEnd);
        }
        self.edge_contact = None;
    }

    /// A peer injected relative motion.
    fn injected_motion(&mut self, dx: f64, dy: f64) {
        self.injected_motions += 1;
        let (x, y) = (self.cursor.0 + dx, self.cursor.1 + dy);
        self.cursor = (x.clamp(0.0, self.max_x()), y.clamp(0.0, self.max_y()));
        let Some(edge) = self.remote_edge else {
            return;
        };
        self.jittery_begin();
        let pushed_past = match edge {
            PointerEdge::Left => x < 0.0,
            PointerEdge::Right => x > self.max_x(),
            PointerEdge::Top => y < 0.0,
            PointerEdge::Bottom => y > self.max_y(),
        };
        if !pushed_past {
            self.edge_contact = None;
            return;
        }
        let entry = to_position(edge);
        if self.personality.injected_crossing_begins_capture
            && self.captured.is_none()
            && self.barriers.contains(&entry)
        {
            self.record(ScreenEvent::Quirk("injected crossing seen by capture"));
            self.begin_capture(entry);
        }
        let now = Instant::now();
        let fire = match &mut self.edge_contact {
            None => {
                self.edge_contact = Some(EdgeContact {
                    last_sent: now,
                    attempts: 1,
                });
                true
            }
            Some(contact)
                if contact.attempts < EDGE_RETURN_MAX_ATTEMPTS
                    && now.duration_since(contact.last_sent) >= EDGE_RETURN_THROTTLE =>
            {
                contact.last_sent = now;
                contact.attempts += 1;
                true
            }
            Some(_) => false,
        };
        if fire {
            let along = self.along(matches!(edge, PointerEdge::Left | PointerEdge::Right));
            self.record(ScreenEvent::EdgeReturn { edge, along });
            if let Some(notifier) = &self.edge_return {
                let _ = notifier.send((edge, Some(along)));
            }
        }
    }
}

impl ScreenState {
    /// [`Personality::begin_jitter_px`]: the capture backend reports a `Begin`
    /// at a barrier the injected pointer merely came close to.
    fn jittery_begin(&mut self) {
        let Some(px) = self.personality.begin_jitter_px else {
            return;
        };
        if self.captured.is_some()
            || self
                .last_jitter
                .is_some_and(|at| at.elapsed() < BEGIN_JITTER_INTERVAL)
        {
            return;
        }
        let (x, y) = self.cursor;
        let near = [
            (Position::Left, x <= px),
            (Position::Right, x >= self.max_x() - px),
            (Position::Top, y <= px),
            (Position::Bottom, y >= self.max_y() - px),
        ]
        .into_iter()
        .find(|&(edge, close)| close && self.barriers.contains(&edge))
        .map(|(edge, _)| edge);
        if let Some(edge) = near {
            self.last_jitter = Some(Instant::now());
            self.record(ScreenEvent::Quirk("jittery begin"));
            self.begin_capture(edge);
        }
    }
}

impl fmt::Display for ScreenState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} [{}] {}x{} cursor=({:.0}, {:.0}) captured={:?} remote_edge={:?} barriers={:?} injected_motions={}",
            self.name,
            self.personality.label,
            self.width,
            self.height,
            self.cursor.0,
            self.cursor.1,
            self.captured,
            self.remote_edge,
            self.barriers,
            self.injected_motions,
        )
    }
}

pub(crate) fn lock(screen: &SharedScreen) -> MutexGuard<'_, ScreenState> {
    screen
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn motion(dx: f64, dy: f64) -> Event {
    Event::Pointer(PointerEvent::Motion { time: 0, dx, dy })
}

fn to_position(edge: PointerEdge) -> Position {
    match edge {
        PointerEdge::Left => Position::Left,
        PointerEdge::Right => Position::Right,
        PointerEdge::Top => Position::Top,
        PointerEdge::Bottom => Position::Bottom,
    }
}

/// Fake capture backend bound to one virtual screen.
pub(crate) struct SimCapture {
    screen: SharedScreen,
    events: UnboundedReceiver<(Position, CaptureEvent)>,
}

impl SimCapture {
    /// A new backend instance takes over the screen's capture channel, like a
    /// recreated OS backend takes over the barriers.
    pub(crate) fn new(screen: SharedScreen) -> Self {
        let (tx, events) = unbounded_channel();
        {
            let mut state = lock(&screen);
            state.capture_tx = Some(tx);
            state.captured = None;
        }
        Self { screen, events }
    }
}

impl Stream for SimCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.events.poll_recv(cx).map(|event| event.map(Ok))
    }
}

#[async_trait]
impl input_capture::Capture for SimCapture {
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        let mut state = lock(&self.screen);
        state.barriers.insert(pos);
        state.record(ScreenEvent::BarrierCreated(pos));
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        let mut state = lock(&self.screen);
        state.barriers.remove(&pos);
        if state.captured == Some(pos) {
            state.release(None);
        }
        state.record(ScreenEvent::BarrierDestroyed(pos));
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        let mut state = lock(&self.screen);
        if state.captured.is_some() {
            state.release(None);
        }
        Ok(())
    }

    async fn release_at(&mut self, pos: Position, along: f32) -> Result<(), CaptureError> {
        let mut state = lock(&self.screen);
        if state.captured.is_some() {
            state.release(Some((pos, along)));
        }
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        let mut state = lock(&self.screen);
        state.barriers.clear();
        state.captured = None;
        state.capture_tx = None;
        Ok(())
    }

    fn set_remote_peer(&mut self, peer: Option<Position>) {
        lock(&self.screen).remote_peer_hint = peer;
    }
}

/// Fake emulation backend bound to one virtual screen.
pub(crate) struct SimEmulation {
    screen: SharedScreen,
}

impl SimEmulation {
    pub(crate) fn new(screen: SharedScreen) -> Self {
        Self { screen }
    }
}

#[async_trait]
impl input_emulation::Emulation for SimEmulation {
    async fn consume(&mut self, event: Event, _: EmulationHandle) -> Result<(), EmulationError> {
        let mut state = lock(&self.screen);
        match event {
            Event::Pointer(PointerEvent::Motion { dx, dy, .. }) => state.injected_motion(dx, dy),
            other => {
                // An injected lock key press toggles the lock like the OS does.
                if let Event::Keyboard(input_event::KeyboardEvent::Key { key, state: 1, .. }) =
                    other
                {
                    if key == crate::lock_sync::KEY_CAPSLOCK || key == crate::lock_sync::KEY_NUMLOCK
                    {
                        state.injected_lock_taps += 1;
                        state.toggle_lock(key);
                    }
                }
                state.injected_other.push(other);
            }
        }
        Ok(())
    }

    async fn create(&mut self, _: EmulationHandle) {}

    async fn destroy(&mut self, _: EmulationHandle) {}

    async fn terminate(&mut self) {
        let mut state = lock(&self.screen);
        state.end_remote();
        state.edge_return = None;
    }

    fn set_edge_return_notifier(&mut self, notifier: EdgeReturnNotifier) {
        lock(&self.screen).edge_return = Some(notifier);
    }

    async fn begin_remote_control(&mut self, edge: PointerEdge, along: Option<f32>) {
        lock(&self.screen).begin_remote(edge, along);
    }

    async fn end_remote_control(&mut self) {
        lock(&self.screen).end_remote();
    }
}
