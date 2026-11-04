use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use input_capture::{
    CaptureError, CaptureEvent, CaptureHandle, InputCapture, InputCaptureError, Position,
};
use input_event::{Event, KeyboardEvent, PointerEvent, scancode};
use lan_mouse_proto::ProtoEvent;
use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::{coalesce::MotionCoalescer, connect::LanMouseConnection};

pub(crate) struct Capture {
    cancellation_token: CancellationToken,
    request_tx: Sender<CaptureRequest>,
    task: JoinHandle<()>,
    event_rx: Receiver<ICaptureEvent>,
    selected_backend: Rc<Cell<Option<input_capture::Backend>>>,
}

pub(crate) enum ICaptureEvent {
    /// a client was entered
    CaptureBegin(CaptureHandle),
    /// capture disabled
    CaptureDisabled,
    /// capture disabled
    CaptureEnabled,
    /// A (new) client was entered.
    /// In contrast to [`ICaptureEvent::CaptureBegin`] this
    /// event is only triggered when the capture was
    /// explicitly released in the meantime by
    /// either the remote client leaving its device region,
    /// a new device entering the screen or the release bind.
    ClientEntered(u64),
    /// The previously active client was left, i.e. capture
    /// was released for the given handle. Mirrors
    /// [`ICaptureEvent::ClientEntered`] for the leave side
    /// and fires on every release path (release-bind chord,
    /// remote `Leave`, explicit `Release` request, send
    /// failure, or destroy of the active capture).
    ClientLeft(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureType {
    /// a normal input capture
    Default,
    /// A capture only interested in [`CaptureEvent::Begin`] events.
    /// The capture is released immediately, if there is no
    /// Default capture at the same position.
    EnterOnly,
}

#[derive(Clone, Debug)]
enum CaptureRequest {
    /// pre-establish the connection for an active client
    Connect(CaptureHandle),
    /// capture must release the mouse
    Release,
    /// add a capture client
    Create(CaptureHandle, Position, CaptureType),
    /// destory a capture client
    Destroy(CaptureHandle),
    /// reenable input capture
    Reenable,
    /// set release bind
    SetReleaseBind(Vec<scancode::Linux>),
    /// scale the motion of a client (1.0 = unchanged)
    SetSensitivity(CaptureHandle, f64),
}

impl Capture {
    pub(crate) fn new(
        backend: Option<input_capture::Backend>,
        conn: LanMouseConnection,
        release_bind: Vec<scancode::Linux>,
        motion_batch_ms: u64,
        connect_grace_ms: u64,
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let selected_backend: Rc<Cell<Option<input_capture::Backend>>> = Default::default();
        let capture_task = CaptureTask {
            active_client: None,
            backend,
            cancellation_token: cancellation_token.clone(),
            captures: Default::default(),
            coalescer: MotionCoalescer::new(motion_batch_ms),
            conn,
            enter: EnterState::default(),
            sensitivity: Default::default(),
            grace: Duration::from_millis(connect_grace_ms),
            disconnected_since: None,
            event_tx,
            request_rx,
            release_bind: Rc::new(RefCell::new(release_bind)),
            selected_backend: selected_backend.clone(),
            state: Default::default(),
        };
        let task = spawn_local(capture_task.run());
        Self {
            cancellation_token,
            request_tx,
            task,
            event_rx,
            selected_backend,
        }
    }

    /// The capture backend that was actually selected, once initialized.
    pub(crate) fn backend(&self) -> Option<input_capture::Backend> {
        self.selected_backend.get()
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(CaptureRequest::Reenable)
            .ok()
            .unwrap_or_default();
    }

    /// Warm up the connection for `handle` so the first capture does not
    /// have to pay the DTLS handshake.
    pub(crate) fn connect(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Connect(handle))
            .ok()
            .unwrap_or_default();
    }

    pub(crate) async fn terminate(&mut self) {
        self.cancellation_token.cancel();
        log::debug!("terminating capture");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }

    pub(crate) fn create(
        &self,
        handle: CaptureHandle,
        pos: lan_mouse_ipc::Position,
        capture_type: CaptureType,
    ) {
        let pos = to_capture_pos(pos);
        self.request_tx
            .send(CaptureRequest::Create(handle, pos, capture_type))
            .ok()
            .unwrap_or_default();
    }

    /// Scale the motion forwarded to a client.
    pub(crate) fn set_sensitivity(&self, handle: CaptureHandle, scale: f64) {
        self.request_tx
            .send(CaptureRequest::SetSensitivity(handle, scale))
            .ok()
            .unwrap_or_default();
    }

    pub(crate) fn destroy(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Destroy(handle))
            .ok()
            .unwrap_or_default();
    }

    pub(crate) fn release(&self) {
        self.request_tx
            .send(CaptureRequest::Release)
            .ok()
            .unwrap_or_default();
    }

    pub(crate) async fn event(&mut self) -> ICaptureEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    pub(crate) fn set_release_bind(&mut self, bind: Vec<scancode::Linux>) {
        let _ = self.request_tx.send(CaptureRequest::SetReleaseBind(bind));
    }
}

/// Retransmit `Enter` until the peer acknowledges it.
const ENTER_RETRY_INTERVAL: Duration = Duration::from_millis(100);
/// Give up on a peer that never acknowledges `Enter` and release the grab.
const ENTER_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the motion coalescing ratio is logged.
const STATS_INTERVAL: Duration = Duration::from_secs(10);

/// debounce a statement `$st`, i.e. the statement is executed only if the
/// time since the previous execution is at least `$dur`.
/// `$prev` is used to keep track of this timestamp
macro_rules! debounce {
    ($prev:ident, $dur:expr, $st:stmt) => {
        let exec = match $prev.get() {
            None => true,
            Some(instant) if instant.elapsed() > $dur => true,
            _ => false,
        };
        if exec {
            $prev.replace(Some(Instant::now()));
            $st
        }
    };
}

/// Handoff state for the `Enter`/`Ack` exchange.
///
/// The capturing side picks a new serial for every capture session and
/// retransmits `Enter` until the peer echoes the serial back in `Ack`.
/// Kept as a small standalone type so the state machine can be tested
/// without a live capture backend.
#[derive(Debug, Default)]
struct EnterState {
    serial: u32,
    awaiting_ack: Option<u32>,
    deadline: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnterTick {
    /// No `Enter` in flight.
    Idle,
    /// An `Enter` is in flight and still within its deadline.
    Retransmit,
    /// The peer never acknowledged in time.
    GiveUp,
}

impl EnterState {
    /// Start a new capture session with a fresh serial.
    fn begin(&mut self, now: Instant, timeout: Duration) {
        self.serial = self.serial.wrapping_add(1);
        self.awaiting_ack = Some(self.serial);
        self.deadline = Some(now + timeout);
    }

    fn serial(&self) -> u32 {
        self.serial
    }

    fn awaiting_ack(&self) -> Option<u32> {
        self.awaiting_ack
    }

    /// Mark the session as acknowledged if `serial` is the one in flight.
    fn ack(&mut self, serial: u32) -> bool {
        if self.awaiting_ack == Some(serial) {
            self.awaiting_ack = None;
            self.deadline = None;
            true
        } else {
            false
        }
    }

    fn tick(&self, now: Instant) -> EnterTick {
        if self.awaiting_ack.is_none() {
            return EnterTick::Idle;
        }
        match self.deadline {
            Some(deadline) if now >= deadline => EnterTick::GiveUp,
            _ => EnterTick::Retransmit,
        }
    }

    fn reset(&mut self) {
        self.awaiting_ack = None;
        self.deadline = None;
    }
}

struct CaptureTask {
    active_client: Option<CaptureHandle>,
    backend: Option<input_capture::Backend>,
    cancellation_token: CancellationToken,
    captures: Vec<(CaptureHandle, Position, CaptureType)>,
    coalescer: MotionCoalescer,
    conn: LanMouseConnection,
    /// `Enter`/`Ack` handoff state
    enter: EnterState,
    /// per-client motion scale (1.0 = unchanged)
    sensitivity: HashMap<CaptureHandle, f64>,
    /// how long a failed motion send may stay down before the capture is
    /// released, matching `connect_grace_ms`
    grace: Duration,
    /// when the active connection was first seen as down
    disconnected_since: Option<Instant>,
    event_tx: Sender<ICaptureEvent>,
    release_bind: Rc<RefCell<Vec<scancode::Linux>>>,
    request_rx: Receiver<CaptureRequest>,
    selected_backend: Rc<Cell<Option<input_capture::Backend>>>,
    state: State,
}

impl CaptureTask {
    fn add_capture(&mut self, handle: CaptureHandle, pos: Position, capture_type: CaptureType) {
        self.captures.push((handle, pos, capture_type));
    }

    fn remove_capture(&mut self, handle: CaptureHandle) {
        self.captures.retain(|&(h, ..)| handle != h);
    }

    fn is_default_capture_at(&self, pos: Position) -> bool {
        self.captures
            .iter()
            .any(|&(_, p, t)| p == pos && t == CaptureType::Default)
    }

    fn get_pos(&self, handle: CaptureHandle) -> Position {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .1
    }

    fn get_type(&self, handle: CaptureHandle) -> CaptureType {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .2
    }

    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_capture().await {
                log::warn!("input capture exited: {e}");
            }
            loop {
                tokio::select! {
                    r = self.request_rx.recv() => match r.expect("channel closed") {
                        CaptureRequest::Connect(h) => self.conn.ensure_connected(h),
                        CaptureRequest::Reenable => break,
                        CaptureRequest::Create(h, p, t) => self.add_capture(h, p, t),
                        CaptureRequest::Destroy(h) => self.remove_capture(h),
                        CaptureRequest::Release => { /* nothing to do */ }
                        CaptureRequest::SetReleaseBind(bind) => {
                            self.release_bind.borrow_mut().clone_from(&bind);
                        }
                        CaptureRequest::SetSensitivity(handle, scale) => {
                            self.sensitivity.insert(handle, scale);
                        }
                    },
                    _ = self.cancellation_token.cancelled() => return,
                }
            }
        }
    }

    async fn do_capture(&mut self) -> Result<(), InputCaptureError> {
        /* allow cancelling capture request */
        let mut capture = tokio::select! {
            r = InputCapture::new(self.backend) => r?,
            _ = self.cancellation_token.cancelled() => return Ok(()),
        };
        self.selected_backend.set(Some(capture.backend()));

        let _capture_guard = DropGuard::new(
            self.event_tx.clone(),
            ICaptureEvent::CaptureEnabled,
            ICaptureEvent::CaptureDisabled,
        );

        /* create barriers for active clients */
        let r = self.create_captures(&mut capture).await;
        if let Err(e) = r {
            capture.terminate().await?;
            return Err(e.into());
        }

        let r = self.do_capture_session(&mut capture).await;

        // FIXME replace with async drop when stabilized
        capture.terminate().await?;

        r
    }

    async fn create_captures(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        let captures = self.captures.clone();
        for (handle, pos, _type) in captures {
            tokio::select! {
                r = capture.create(handle, pos) => r?,
                _ = self.cancellation_token.cancelled() => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_capture_session(
        &mut self,
        capture: &mut InputCapture,
    ) -> Result<(), InputCaptureError> {
        // Flush coalesced motion on its own cadence. The first tick of a
        // `tokio::time::interval` completes immediately, which is what we
        // want when a previous session left something pending.
        let mut flush_timer = self.coalescer.interval().map(|batch| {
            let mut timer = tokio::time::interval(batch);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            timer
        });

        // Retransmit `Enter` until the peer acknowledges it. A single lost
        // `Enter` otherwise leaves the peer holding its own capture (or
        // never releasing it) until the next crossing.
        let mut enter_retry = tokio::time::interval(ENTER_RETRY_INTERVAL);
        enter_retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        enter_retry.tick().await;

        // Report the coalescing ratio periodically so the effect of
        // `motion_batch_ms` can be verified from the logs.
        let mut stats_interval = tokio::time::interval(STATS_INTERVAL);
        stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        stats_interval.tick().await;

        loop {
            tokio::select! {
                _ = stats_interval.tick() => {
                    let (events_in, batches_out) = self.coalescer.counters();
                    if events_in > 0 {
                        log::debug!(
                            "motion stats: {events_in} events in, {batches_out} packets out \
                             ({:.1} events/packet)",
                            events_in as f64 / batches_out.max(1) as f64
                        );
                    }
                },
                _ = async {
                    match flush_timer.as_mut() {
                        Some(timer) => { timer.tick().await; }
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    if !self.flush_motion().await {
                        log::warn!("releasing capture: failed to send coalesced motion");
                        self.release_capture(capture).await?;
                    }
                },
                _ = enter_retry.tick() => {
                    self.retransmit_enter(capture).await?;
                    if let Some(since) = self.disconnected_since {
                        if self.active_client.is_some() && since.elapsed() >= self.grace {
                            log::warn!(
                                "releasing capture: connection stayed down for {:?}",
                                since.elapsed()
                            );
                            self.disconnected_since = None;
                            self.release_capture(capture).await?;
                        }
                    }
                },
                event = capture.next() => match event {
                    Some(event) => self.handle_capture_event(capture, event?).await?,
                    None => return Ok(()),
                },
                (handle, event) = self.conn.recv() => {
                    if let Some(active) = self.active_client {
                        if handle != active {
                            // we only care about events coming from the client we are currently connected to
                            // only `Ack` and `Leave` are relevant
                            continue
                        }
                    }

                    match event {
                        // connection acknowlegded => set state to Sending
                        ProtoEvent::Ack(serial) => {
                            if self.enter.ack(serial) {
                                log::info!("client {handle} acknowledged the connection!");
                                self.state = State::Sending;
                                if !self.flush_motion().await {
                                    log::warn!(
                                        "releasing capture: failed to send buffered motion"
                                    );
                                    self.release_capture(capture).await?;
                                }
                            } else {
                                log::debug!(
                                    "ignoring stale Ack({serial}) from client {handle} (waiting for {:?})",
                                    self.enter.awaiting_ack()
                                );
                            }
                        }
                        // client disconnected
                        ProtoEvent::Leave(_) => {
                            log::info!("releasing capture: left remote client device region");
                            self.release_capture(capture).await?;
                        },
                        _ => {}
                    }
                },
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    CaptureRequest::Connect(h) => self.conn.ensure_connected(h),
                    CaptureRequest::SetSensitivity(h, scale) => {
                        self.sensitivity.insert(h, scale);
                    }
                    CaptureRequest::Reenable => { /* already active */ },
                    CaptureRequest::Release => self.release_capture(capture).await?,
                    CaptureRequest::Create(h, p, t) => {
                        self.add_capture(h, p, t);
                        capture.create(h, p).await?;
                    }
                    CaptureRequest::Destroy(h) => {
                        // If the capture we're tearing down is the
                        // currently-active one, treat this as a
                        // release for hook purposes. The release_capture
                        // path also clears active_client and flushes
                        // pressed-key state to the peer; without this,
                        // `cli deactivate` (or a hostname change
                        // re-creating the client) would skip leave_hook.
                        if self.active_client == Some(h) {
                            self.release_capture(capture).await?;
                        }
                        self.remove_capture(h);
                        capture.destroy(h).await?;
                    }
                    CaptureRequest::SetReleaseBind(bind) => {
                        self.release_bind.borrow_mut().clone_from(&bind);
                    }
                },
                _ = self.cancellation_token.cancelled() => break,
            }
        }
        Ok(())
    }

    async fn handle_capture_event(
        &mut self,
        capture: &mut InputCapture,
        event: (CaptureHandle, CaptureEvent),
    ) -> Result<(), CaptureError> {
        let (handle, event) = event;
        log::trace!("({handle}): {event:?}");

        if capture.keys_pressed(&self.release_bind.borrow()) {
            log::info!("releasing capture: release-bind pressed");
            return self.release_capture(capture).await;
        }

        if event == CaptureEvent::Begin {
            self.event_tx
                .send(ICaptureEvent::CaptureBegin(handle))
                .ok()
                .unwrap_or_default();
        }

        // enter only capture (for incoming connections)
        if self.get_type(handle) == CaptureType::EnterOnly {
            // if there is no active outgoing connection at the current capture,
            // we release the capture
            if !self.is_default_capture_at(self.get_pos(handle)) {
                log::info!("releasing capture: no active client at this position");
                capture.release().await?;
            }
            // we dont care about events from incoming handles except for releasing the capture
            return Ok(());
        }

        // activated a new client
        if event == CaptureEvent::Begin && Some(handle) != self.active_client {
            self.active_client.replace(handle);
            if self.conn.peer_supports_enter_ack(handle) {
                // The peer echoes the serial back, so a lost `Enter` can be
                // retransmitted and a duplicate one can be dropped.
                self.state = State::WaitingForAck;
                self.enter.begin(Instant::now(), ENTER_TIMEOUT);
            } else {
                // Legacy peer (e.g. any upstream release): it never
                // acknowledges `Enter`. Waiting for an `Ack` would release
                // the grab, immediately re-grab at the barrier and fight the
                // peer for the pointer, freezing both machines.
                log::debug!("client {handle} is a legacy peer: fire-and-forget Enter");
                self.enter.reset();
                self.state = State::Sending;
            }
            self.event_tx
                .send(ICaptureEvent::ClientEntered(handle))
                .ok()
                .unwrap_or_default();
        }

        // Trim the speed for this client: Wayland (layer-shell) reports raw
        // deltas while Windows/macOS report accelerated ones, so each peer
        // combination may need a different scale.
        let event = match event {
            CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { time, dx, dy })) => {
                let scale = self.sensitivity.get(&handle).copied().unwrap_or(1.0);
                if scale == 1.0 {
                    event
                } else {
                    CaptureEvent::Input(Event::Pointer(PointerEvent::Motion {
                        time,
                        dx: dx * scale,
                        dy: dy * scale,
                    }))
                }
            }
            other => other,
        };

        // Coalesce pointer motion while a client is active. Motion that
        // arrives while we are still waiting for `Ack` is buffered and
        // flushed as soon as the acknowledgement arrives, so the brief
        // handshake window does not drop the first movements.
        if self.coalescer.enabled() && self.active_client == Some(handle) {
            if let CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { time, dx, dy })) =
                event
            {
                let due = self.coalescer.push(handle, time, dx, dy, Instant::now());
                if due && !self.flush_motion().await {
                    log::warn!("releasing capture: failed to send coalesced motion");
                    return self.release_capture(capture).await;
                }
                return Ok(());
            }
        }

        // Preserve ordering: never let a discrete event overtake pending
        // motion. `Begin` is excluded because it is the *start* of a new
        // session and any pending motion belongs to the old one.
        if self.active_client == Some(handle)
            && event != CaptureEvent::Begin
            && !self.flush_handle(handle).await
        {
            return self.release_capture(capture).await;
        }

        let opposite_pos = to_proto_pos(self.get_pos(handle).opposite());
        let enter_serial = self.enter.serial();

        let event = match event {
            CaptureEvent::Begin => ProtoEvent::Enter {
                pos: opposite_pos,
                serial: enter_serial,
            },
            CaptureEvent::Input(e) => match self.state {
                // connection not acknowledged, repeat `Enter` event
                State::WaitingForAck => ProtoEvent::Enter {
                    pos: opposite_pos,
                    serial: enter_serial,
                },
                State::Sending => ProtoEvent::Input(e),
            },
        };

        if let Err(e) = self.conn.send(event, handle).await {
            const DUR: Duration = Duration::from_millis(500);
            debounce!(PREV_LOG, DUR, log::warn!("releasing capture: {e}"));
            // Funnel through release_capture so the leave_hook
            // fires and active_client is cleared (without this the
            // active_client field would stay stale until the next
            // Begin from a different handle).
            self.release_capture(capture).await?;
        }
        Ok(())
    }

    /// Send and clear the pending motion of a single handle.
    /// Returns `false` if the send failed hard (the caller should release).
    async fn flush_handle(&mut self, handle: CaptureHandle) -> bool {
        let Some(motion) = self.coalescer.take(handle) else {
            return true;
        };
        self.send_motion(handle, motion).await
    }

    /// Send and clear every pending motion (timer tick and release paths).
    /// Returns `false` if any send failed hard.
    async fn flush_motion(&mut self) -> bool {
        let mut ok = true;
        for (handle, motion) in self.coalescer.take_all() {
            ok &= self.send_motion(handle, motion).await;
        }
        ok
    }

    /// Drain pending motion without waiting for a (re)connection.
    async fn flush_motion_now(&mut self) {
        for (handle, motion) in self.coalescer.take_all() {
            let event = ProtoEvent::Input(Event::Pointer(PointerEvent::Motion {
                time: motion.time,
                dx: motion.dx,
                dy: motion.dy,
            }));
            if let Err(e) = self.conn.send_now(event, handle).await {
                log::warn!("failed to flush motion to client {handle}: {e}");
            }
        }
    }

    async fn send_motion(
        &mut self,
        handle: CaptureHandle,
        motion: crate::coalesce::Motion,
    ) -> bool {
        let event = ProtoEvent::Input(Event::Pointer(PointerEvent::Motion {
            time: motion.time,
            dx: motion.dx,
            dy: motion.dy,
        }));
        // Never wait for a (re)connection here: a motion event that is a few
        // hundred milliseconds old is useless, and blocking the capture loop
        // on it queued up hundreds of events that then arrived in one jump.
        // The capture is released after `grace` instead.
        match self.conn.send_now(event, handle).await {
            Ok(()) => {
                self.disconnected_since = None;
                true
            }
            Err(e) => {
                log::debug!("dropping motion for client {handle}: {e}");
                if self.disconnected_since.is_none() {
                    self.disconnected_since = Some(Instant::now());
                }
                self.conn.ensure_connected(handle);
                true
            }
        }
    }

    /// Retransmit the current `Enter` until it is acknowledged, and release
    /// the capture if the peer never answers.
    async fn retransmit_enter(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        match self.enter.tick(Instant::now()) {
            EnterTick::Idle => return Ok(()),
            EnterTick::GiveUp => {
                if let Some(handle) = self.active_client {
                    log::warn!("client {handle} did not acknowledge Enter, releasing capture");
                }
                return self.release_capture(capture).await;
            }
            EnterTick::Retransmit => {}
        }
        let Some(handle) = self.active_client else {
            return Ok(());
        };
        let pos = to_proto_pos(self.get_pos(handle).opposite());
        let serial = self.enter.serial();
        if let Err(e) = self
            .conn
            .send(ProtoEvent::Enter { pos, serial }, handle)
            .await
        {
            log::debug!("failed to retransmit Enter to client {handle}: {e}");
        }
        Ok(())
    }

    async fn release_capture(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        // Deliver pending motion before the synthesized key-up events and
        // `Leave` so the peer never observes an out-of-order button/key.
        // Teardown uses the non-blocking send path: releasing a connection
        // to a dead peer must not keep the input grabbed for the full
        // connection grace. Failure is expected here and must not recurse
        // into another release.
        self.flush_motion_now().await;
        self.state = State::WaitingForAck;
        self.enter.reset();

        // If we have an active client, notify them we're leaving
        if let Some(handle) = self.active_client.take() {
            // Surface the leave to the service layer so it can fire
            // the per-client leave_hook. Sent before the network
            // teardown below so we never race against the peer
            // disappearing.
            self.event_tx
                .send(ICaptureEvent::ClientLeft(handle))
                .ok()
                .unwrap_or_default();
            // Synthesize key-up events for every key still held in the
            // capture's pressed_keys set BEFORE sending Leave. Without
            // this, pressing the release-bind chord (typically all four
            // modifiers) leaves the peer with phantom held modifiers:
            // the down events were forwarded while capture was active,
            // but the matching up events arrive after the local tap
            // flips to passthrough and never reach the peer. The peer
            // then runs every subsequent keystroke through those held
            // mods until its watchdog times out (1+ s) or our Leave
            // arrives — and Leave can be lost over UDP/DTLS.
            for key in capture.take_pressed_keys() {
                let key_up = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: key as u32,
                    state: 0,
                }));
                if let Err(e) = self.conn.send_now(key_up, handle).await {
                    log::warn!("failed to send key-up to client {handle}: {e}");
                }
            }
            // Reset the modifier mask too. The peer's input-emulation
            // layer keeps a separate XKB-style modifier state that's
            // updated by KeyboardEvent::Modifiers, distinct from the
            // pressed_keys set drained above. Without this, an
            // already-locked CapsLock would survive the release.
            let mods_zero = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            }));
            if let Err(e) = self.conn.send_now(mods_zero, handle).await {
                log::warn!("failed to reset modifiers on client {handle}: {e}");
            }

            log::info!("sending Leave event to client {handle}");
            if let Err(e) = self.conn.send_now(ProtoEvent::Leave(0), handle).await {
                log::warn!("failed to send Leave to client {handle}: {e}");
            }
        }
        capture.release().await
    }
}

thread_local! {
    static PREV_LOG: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    WaitingForAck,
    Sending,
}

fn to_capture_pos(pos: lan_mouse_ipc::Position) -> input_capture::Position {
    match pos {
        lan_mouse_ipc::Position::Left => input_capture::Position::Left,
        lan_mouse_ipc::Position::Right => input_capture::Position::Right,
        lan_mouse_ipc::Position::Top => input_capture::Position::Top,
        lan_mouse_ipc::Position::Bottom => input_capture::Position::Bottom,
    }
}

fn to_proto_pos(pos: input_capture::Position) -> lan_mouse_proto::Position {
    match pos {
        input_capture::Position::Left => lan_mouse_proto::Position::Left,
        input_capture::Position::Right => lan_mouse_proto::Position::Right,
        input_capture::Position::Top => lan_mouse_proto::Position::Top,
        input_capture::Position::Bottom => lan_mouse_proto::Position::Bottom,
    }
}

struct DropGuard<T> {
    tx: Sender<T>,
    on_drop: Option<T>,
}

impl<T> DropGuard<T> {
    fn new(tx: Sender<T>, on_new: T, on_drop: T) -> Self {
        tx.send(on_new).ok().unwrap_or_default();
        let on_drop = Some(on_drop);
        Self { tx, on_drop }
    }
}

impl<T> Drop for DropGuard<T> {
    fn drop(&mut self) {
        self.tx
            .send(self.on_drop.take().expect("item"))
            .ok()
            .unwrap_or_default();
    }
}

#[cfg(test)]
mod enter_state_tests {
    use super::*;

    const TIMEOUT: Duration = Duration::from_millis(100);

    #[test]
    fn begin_increments_the_serial() {
        let mut state = EnterState::default();
        let now = Instant::now();
        assert_eq!(state.serial(), 0);
        assert_eq!(state.tick(now), EnterTick::Idle);

        state.begin(now, TIMEOUT);
        assert_eq!(state.serial(), 1);
        assert_eq!(state.awaiting_ack(), Some(1));
        assert_eq!(state.tick(now), EnterTick::Retransmit);

        state.begin(now, TIMEOUT);
        assert_eq!(state.serial(), 2);
        assert_eq!(state.awaiting_ack(), Some(2));
    }

    #[test]
    fn serial_wraps_around() {
        let mut state = EnterState {
            serial: u32::MAX,
            ..Default::default()
        };
        state.begin(Instant::now(), TIMEOUT);
        assert_eq!(state.serial(), 0);
    }

    #[test]
    fn ack_matches_only_the_in_flight_serial() {
        let mut state = EnterState::default();
        let now = Instant::now();
        state.begin(now, TIMEOUT);

        assert!(!state.ack(0), "stale serial must be ignored");
        assert_eq!(state.awaiting_ack(), Some(1));
        assert!(state.ack(1));
        assert_eq!(state.awaiting_ack(), None);
        assert_eq!(state.tick(now), EnterTick::Idle);
    }

    #[test]
    fn tick_retransmits_until_the_deadline_then_gives_up() {
        let mut state = EnterState::default();
        let now = Instant::now();
        state.begin(now, TIMEOUT);

        assert_eq!(state.tick(now), EnterTick::Retransmit);
        assert_eq!(
            state.tick(now + TIMEOUT / 2),
            EnterTick::Retransmit,
            "still inside the window"
        );
        assert_eq!(
            state.tick(now + TIMEOUT),
            EnterTick::GiveUp,
            "deadline is inclusive"
        );
        assert_eq!(state.tick(now + TIMEOUT * 2), EnterTick::GiveUp);
    }

    #[test]
    fn reset_clears_the_pending_handoff() {
        let mut state = EnterState::default();
        let now = Instant::now();
        state.begin(now, TIMEOUT);
        state.reset();
        assert_eq!(state.awaiting_ack(), None);
        assert_eq!(state.tick(now + TIMEOUT * 2), EnterTick::Idle);
        assert!(!state.ack(1), "a reset handoff cannot be acknowledged");
        assert_eq!(state.serial(), 1, "the serial is not reused");
    }

    #[test]
    fn a_new_session_after_reset_uses_the_next_serial() {
        let mut state = EnterState::default();
        let now = Instant::now();
        state.begin(now, TIMEOUT);
        state.reset();
        state.begin(now, TIMEOUT);
        assert_eq!(state.serial(), 2);
        assert_eq!(state.awaiting_ack(), Some(2));
    }
}
