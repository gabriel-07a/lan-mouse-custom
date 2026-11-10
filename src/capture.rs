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
    /// explicit user disconnect: revoke/close the outgoing transport
    Disconnect(CaptureHandle),
    /// close the outgoing transport without notifying the peer (the client
    /// entry was deleted/merged: the peer must not see an explicit disconnect)
    DropTransport(CaptureHandle),
    /// reenable input capture
    Reenable,
    /// set release bind
    SetReleaseBind(Vec<scancode::Linux>),
    /// scale the motion of a client (1.0 = unchanged)
    SetSensitivity(CaptureHandle, f64),
    /// replace the capabilities this machine may use on the client
    SetOutgoingCaps(CaptureHandle, lan_mouse_ipc::Capabilities),
    /// edge a remote peer entered through while it drives this machine;
    /// `None` means no peer is driving us
    SetRemotePeer(Option<lan_mouse_ipc::Position>),
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
            outgoing_caps: Default::default(),
            conn,
            enter: EnterState::default(),
            last_enter_pair: None,
            sensitivity: Default::default(),
            remote_peer: None,
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
    /// Update the capabilities allowed in the outgoing direction for a client.
    ///
    /// Events that need a capability the user did not grant are dropped by
    /// the capture task instead of being sent to the peer.
    pub(crate) fn set_outgoing_caps(
        &self,
        handle: CaptureHandle,
        caps: lan_mouse_ipc::Capabilities,
    ) {
        self.request_tx
            .send(CaptureRequest::SetOutgoingCaps(handle, caps))
            .ok()
            .unwrap_or_default();
    }

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

    /// Close the outgoing transport of a client after an explicit user
    /// disconnect. The service marks the client inactive first so the
    /// reconnect loop does not restart it.
    pub(crate) fn disconnect(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Disconnect(handle))
            .ok()
            .unwrap_or_default();
    }

    /// Close the outgoing transport of a client without telling the peer.
    ///
    /// Used when the client entry disappears (deleted, config reload, merged
    /// duplicate): an explicit-disconnect notification would deactivate the
    /// peer's own configured client for us, which is not what a local bookkeeping
    /// change means. The service removes the client from the manager first, so
    /// the connection layer sees it as inactive and does not reconnect.
    pub(crate) fn drop_transport(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::DropTransport(handle))
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

    /// A remote peer started driving this machine from `peer`'s edge, or
    /// stopped driving it (`None`).
    pub(crate) fn set_remote_peer(&self, peer: Option<lan_mouse_ipc::Position>) {
        let _ = self.request_tx.send(CaptureRequest::SetRemotePeer(peer));
    }
}

/// Base cadence for checking `Enter` retransmission and connection grace expiry.
const ENTER_RETRY_TICK: Duration = Duration::from_millis(25);
/// Minimum adaptive `Enter` retransmission interval.
const ENTER_RETRY_MIN: Duration = Duration::from_millis(25);
/// Maximum adaptive `Enter` retransmission interval on high-RTT links.
const ENTER_RETRY_MAX: Duration = Duration::from_millis(100);
/// Default `Enter` retransmission interval before the first RTT sample arrives.
const ENTER_RETRY_DEFAULT: Duration = Duration::from_millis(35);
/// Minimum interval between `Enter` packets paired with input while the
/// hand-off is still unacknowledged. The regular retry timer only fires every
/// 25-100 ms, which would stall the first click or movement after a lost
/// `Enter` for that long; pairing repairs it within a couple of milliseconds
/// without flooding the link with hand-off packets.
const ENTER_PAIR_THROTTLE: Duration = Duration::from_millis(5);
/// Maximum time a discrete event (`Button`, `Keyboard`, `Enter`) may wait for
/// an in-flight connection attempt on a single capture loop iteration before
/// falling back to the `disconnected_since` grace tracker.
const DISCRETE_SEND_BUDGET: Duration = Duration::from_millis(15);
/// Give up on a peer that never acknowledges `Enter` and release the grab.
const ENTER_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the motion coalescing ratio is logged.
const STATS_INTERVAL: Duration = Duration::from_secs(10);

/// Compute the adaptive `Enter` retransmission interval from the smoothed RTT.
///
/// Uses `clamp(2 * rtt, 25ms, 100ms)` when RTT is known and `35ms` otherwise so
/// a lost `Enter` on a LAN/Wi-Fi link recovers quickly without flooding a slow
/// or congested link with duplicate handoff packets.
fn adaptive_enter_retry_interval(rtt_ms: Option<u32>) -> Duration {
    match rtt_ms {
        Some(rtt) => Duration::from_millis(u64::from(rtt).saturating_mul(2))
            .clamp(ENTER_RETRY_MIN, ENTER_RETRY_MAX),
        None => ENTER_RETRY_DEFAULT,
    }
}

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
    last_sent: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnterTick {
    /// No `Enter` in flight, or the retry interval has not elapsed yet.
    Idle,
    /// An `Enter` is in flight and due for retransmission.
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
        self.last_sent = Some(now);
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
            self.last_sent = None;
            true
        } else {
            false
        }
    }

    fn tick(&mut self, now: Instant, retry_interval: Duration) -> EnterTick {
        if self.awaiting_ack.is_none() {
            return EnterTick::Idle;
        }
        match self.deadline {
            Some(deadline) if now >= deadline => EnterTick::GiveUp,
            _ => {
                let due = self
                    .last_sent
                    .is_none_or(|last| now.saturating_duration_since(last) >= retry_interval);
                if due {
                    self.last_sent = Some(now);
                    EnterTick::Retransmit
                } else {
                    EnterTick::Idle
                }
            }
        }
    }

    fn reset(&mut self) {
        self.awaiting_ack = None;
        self.deadline = None;
        self.last_sent = None;
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
    /// Last time an unacknowledged `Enter` was paired with input, throttled by
    /// [`ENTER_PAIR_THROTTLE`] so the retransmission is repaired quickly
    /// without flooding the link.
    last_enter_pair: Option<Instant>,
    /// per-client motion scale (1.0 = unchanged)
    sensitivity: HashMap<CaptureHandle, f64>,
    /// capabilities allowed in the outgoing direction, per client
    outgoing_caps: HashMap<CaptureHandle, lan_mouse_ipc::Capabilities>,
    /// edge a remote peer entered through while it drives this machine
    /// (`None` = no peer is driving us)
    remote_peer: Option<lan_mouse_ipc::Position>,
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

    // Position of a registered capture, if it still exists.
    //
    // A backend event can legitimately arrive for a handle that was destroyed
    // in the meantime: when two captures share a screen edge (a Default one
    // and the incoming EnterOnly one), InputCapture fans one backend event out
    // to every id at that position and queues the copies; a Destroy request
    // processed before the queued copy is consumed leaves the copy pointing at
    // a dead handle. Dropping such an event is the only correct behaviour
    // (panicking took the whole service down).
    fn get_pos(&self, handle: CaptureHandle) -> Option<Position> {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .map(|(_, pos, _)| *pos)
    }

    // Capture type of a registered capture, if it still exists.
    fn get_type(&self, handle: CaptureHandle) -> Option<CaptureType> {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .map(|(_, _, capture_type)| *capture_type)
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
                        CaptureRequest::Disconnect(h) => self.conn.disconnect(h).await,
                        CaptureRequest::DropTransport(h) => self.conn.drop_transport(h).await,
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
                        CaptureRequest::SetOutgoingCaps(handle, caps) => {
                            self.outgoing_caps.insert(handle, caps);
                        }
                        CaptureRequest::SetRemotePeer(peer) => {
                            self.remote_peer = peer;
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
        capture.set_remote_peer(self.remote_peer.map(to_capture_pos));
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
        let mut enter_retry = tokio::time::interval(ENTER_RETRY_TICK);
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
                        // The peer asks us to release our capture (return
                        // hand-off) or confirms that it already released its
                        // own. A non-zero serial marks a request the peer
                        // retries until it receives our confirmation, so if we
                        // no longer hold a capture we answer it again instead
                        // of silently dropping it (a single lost Leave used to
                        // leave the peer's remote-control state latched, with
                        // the cursor stuck on its screen).
                        ProtoEvent::Leave(serial) => {
                            if self.active_client == Some(handle) {
                                // A non-zero serial is an explicit return
                                // hand-off request. `Leave(0)` from a peer that
                                // advertises serialized requests is only the
                                // confirmation of that peer releasing its own
                                // capture (sent on every `Enter`); honouring it
                                // cancelled the capture we just acquired and
                                // bounced control between the machines at the
                                // shared edge. Legacy peers use `Leave(0)` as
                                // the request, so they keep the old behaviour.
                                if leave_releases_capture(
                                    serial,
                                    self.conn.peer_supports_serialized_leave(handle),
                                ) {
                                    log::info!(
                                        "releasing capture: left remote client device region (Leave({serial}))"
                                    );
                                    self.release_capture(capture).await?;
                                } else {
                                    log::debug!(
                                        "ignoring release confirmation Leave(0) from client {handle}"
                                    );
                                }
                            } else if serial != 0 {
                                log::debug!(
                                    "re-confirming capture release to {handle} (retried request {serial})"
                                );
                                if let Err(e) =
                                    self.conn.send_now(ProtoEvent::Leave(0), handle).await
                                {
                                    log::debug!("failed to re-confirm release: {e}");
                                }
                            }
                        },
                        _ => {}
                    }
                },
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    CaptureRequest::Connect(h) => self.conn.ensure_connected(h),
                    CaptureRequest::Disconnect(h) => self.conn.disconnect(h).await,
                    CaptureRequest::DropTransport(h) => self.conn.drop_transport(h).await,
                    CaptureRequest::SetSensitivity(h, scale) => {
                        self.sensitivity.insert(h, scale);
                    }
                    CaptureRequest::SetOutgoingCaps(h, caps) => {
                        self.outgoing_caps.insert(h, caps);
                    }
                    CaptureRequest::SetRemotePeer(peer) => {
                        self.remote_peer = peer;
                        capture.set_remote_peer(peer.map(to_capture_pos));
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
                _ = self.cancellation_token.cancelled() => {
                    // Ctrl-C / SIGTERM / GUI close while controlling a peer:
                    // release the grab and send the synthesized key-ups and
                    // Leave, otherwise the peer keeps phantom-held keys and a
                    // grabbed pointer until its watchdog fires.
                    if self.active_client.is_some() {
                        if let Err(e) = self.release_capture(capture).await {
                            log::warn!("failed to release the capture on shutdown: {e}");
                        }
                    }
                    break;
                },
            }
        }
        Ok(())
    }

    /// Whether the client's outgoing policy allows this capture event.
    fn outgoing_event_allowed(&self, handle: CaptureHandle, event: &CaptureEvent) -> bool {
        use lan_mouse_ipc::Capabilities;
        let Some(caps) = self.outgoing_caps.get(&handle).copied() else {
            // No policy registered yet: keep the previous (permissive)
            // behavior until the service pushes the real policy.
            return true;
        };
        match event {
            CaptureEvent::Begin => {
                caps.intersects(Capabilities::POINTER) || caps.intersects(Capabilities::KEYBOARD)
            }
            CaptureEvent::Input(Event::Pointer(_)) => caps.contains(Capabilities::POINTER),
            CaptureEvent::Input(Event::Keyboard(_)) => caps.contains(Capabilities::KEYBOARD),
        }
    }

    async fn handle_capture_event(
        &mut self,
        capture: &mut InputCapture,
        event: (CaptureHandle, CaptureEvent),
    ) -> Result<(), CaptureError> {
        let (handle, event) = event;
        log::trace!("({handle}): {event:?}");

        // The capture may have been destroyed between the backend queueing the
        // event and this task consuming it (see get_pos); ignore stale events
        // instead of panicking.
        let Some(capture_type) = self.get_type(handle) else {
            log::debug!("dropping {event:?} for unknown capture handle {handle}");
            return Ok(());
        };

        // While a peer drives this machine, a barrier begin on an outgoing
        // capture is the injected pointer being pushed back toward the peer.
        // Only the incoming (EnterOnly) capture may act on it: it sends the
        // `Leave` that releases the peer's grab. Starting the outgoing capture
        // here would reflect the injected motion back to the peer and loop.
        if is_suppressed_remote_begin(self.remote_peer.map(to_capture_pos), capture_type, &event) {
            log::debug!("ignoring outgoing capture begin while a peer drives this machine");
            // The capture backend may already have activated a capture for
            // this begin (Windows edge activation hides the pointer and sets
            // ACTIVE_CLIENT). Discarding the event without releasing would
            // leave the hidden pointer/ACTIVE_CLIENT latched when the peer's
            // Leave arrives: the next local inputs would then keep retrying
            // Enter against a stale handle. Undo the activation here; the
            // actual return hand-off is still driven by the emulation
            // EdgeReturn/Leave path.
            capture.release().await?;
            return Ok(());
        }

        if capture.keys_pressed(&self.release_bind.borrow()) {
            log::info!("releasing capture: release-bind pressed");
            return self.release_capture(capture).await;
        }

        if !self.outgoing_event_allowed(handle, &event) {
            log::debug!(
                "dropping {event:?} for client {handle}: capability not granted for the outgoing direction"
            );
            return Ok(());
        }

        if event == CaptureEvent::Begin {
            self.event_tx
                .send(ICaptureEvent::CaptureBegin(handle))
                .ok()
                .unwrap_or_default();
        }

        // enter only capture (for incoming connections)
        if capture_type == CaptureType::EnterOnly {
            // if there is no active outgoing connection at the current capture,
            // we release the capture
            let Some(pos) = self.get_pos(handle) else {
                return Ok(());
            };
            if !self.is_default_capture_at(pos) {
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
                self.last_enter_pair = None;
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
        // arrives while we are still waiting for `Ack` is buffered and flushed
        // on the normal cadence (a lost `Enter` is repaired ahead of it by
        // `pair_enter_if_due`), so the handshake window does not drop the first
        // movements.
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

        let Some(pos) = self.get_pos(handle) else {
            return Ok(());
        };
        let opposite_pos = to_proto_pos(pos.opposite());
        let enter_serial = self.enter.serial();
        let is_begin = event == CaptureEvent::Begin;

        // Old behaviour rewrote every input event into a duplicate `Enter`
        // until the peer acknowledged the hand-off, silently dropping clicks,
        // wheel ticks and key presses that happened within the first round
        // trip (or longer, when `Enter`/`Ack` was lost). The receiver accepts
        // input as soon as the first `Enter` arrived and deduplicates repeated
        // serials, so pair the event with a repair `Enter` and forward it.
        if !is_begin {
            self.pair_enter_if_due(handle).await;
        }

        let event = match event {
            CaptureEvent::Begin => ProtoEvent::Enter {
                pos: opposite_pos,
                serial: enter_serial,
            },
            CaptureEvent::Input(e) => ProtoEvent::Input(e),
        };

        // Scroll/axis events are high-frequency streams like pointer motion:
        // waiting on a reconnect for each wheel tick would stall the capture
        // select loop and replay a burst later. Send them non-blockingly and
        // let `disconnected_since` handle prolonged outages.
        if matches!(
            event,
            ProtoEvent::Input(Event::Pointer(
                PointerEvent::Axis { .. }
                    | PointerEvent::AxisDiscrete120 { .. }
                    | PointerEvent::Motion { .. }
            ))
        ) {
            self.send_unreliable(handle, event).await;
            return Ok(());
        }

        // Discrete events (Enter, Button, Keyboard) wait for at most a short
        // bounded budget (`DISCRETE_SEND_BUDGET`) per event so a transient
        // socket glitch does not freeze the capture loop for the full 400ms
        // grace window. If the link stays down past `self.grace`, the capture
        // is released cleanly.
        let budget = self.grace.min(DISCRETE_SEND_BUDGET);
        // A release lost on the Wi-Fi leaves the peer's button/key latched;
        // retransmit it once. Peers that deduplicate retransmissions (the same
        // builds that acknowledge `Enter`) drop the duplicate, so the second
        // datagram has no side effect; older peers would inject it twice.
        let redundant_release =
            is_release_event(&event) && self.conn.peer_supports_enter_ack(handle);
        match self.conn.send_with_grace(event, handle, budget).await {
            Ok(()) => {
                self.disconnected_since = None;
                // A single lost `Enter` stalls the hand-off until the retry
                // timer. Sending a second copy on the same path mirrors the
                // redundant `Leave(0)` and makes the common case survive an
                // isolated packet loss; the receiver re-acks instead of
                // re-applying it.
                if is_begin && self.state == State::WaitingForAck {
                    let _ = self
                        .conn
                        .send_now(
                            ProtoEvent::Enter {
                                pos: opposite_pos,
                                serial: enter_serial,
                            },
                            handle,
                        )
                        .await;
                }
                if redundant_release {
                    let _ = self.conn.send_now(event, handle).await;
                }
            }
            Err(e) => {
                let now = Instant::now();
                let since = *self.disconnected_since.get_or_insert(now);
                self.conn.ensure_connected(handle);
                if self.grace.is_zero() || now.saturating_duration_since(since) >= self.grace {
                    const DUR: Duration = Duration::from_millis(500);
                    debounce!(PREV_LOG, DUR, log::warn!("releasing capture: {e}"));
                    // Funnel through release_capture so the leave_hook
                    // fires and active_client is cleared (without this the
                    // active_client field would stay stale until the next
                    // Begin from a different handle).
                    self.release_capture(capture).await?;
                } else {
                    log::debug!(
                        "discrete send to client {handle} deferred ({e}); within grace window"
                    );
                }
            }
        }
        Ok(())
    }

    /// Pair a retransmission of the still-unacknowledged `Enter` with the next
    /// input packet, if the throttle allows it.
    ///
    /// The adaptive retry timer only fires every 25-100 ms. A click or the
    /// first movement after a crossing usually happens sooner, and when the
    /// original `Enter` was lost the receiver cannot apply that input until a
    /// retransmission arrives. Sending the repair right before the input keeps
    /// the worst case down to [`ENTER_PAIR_THROTTLE`] without a retransmission
    /// flood: the receiver deduplicates the serial and only re-acks it.
    async fn pair_enter_if_due(&mut self, handle: CaptureHandle) {
        if self.state != State::WaitingForAck
            || self.enter.awaiting_ack().is_none()
            || self.active_client != Some(handle)
        {
            return;
        }
        if self
            .last_enter_pair
            .is_some_and(|last| last.elapsed() < ENTER_PAIR_THROTTLE)
        {
            return;
        }
        let Some(pos) = self.get_pos(handle) else {
            return;
        };
        self.last_enter_pair = Some(Instant::now());
        let event = ProtoEvent::Enter {
            pos: to_proto_pos(pos.opposite()),
            serial: self.enter.serial(),
        };
        let _ = self.conn.send_now(event, handle).await;
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
            // Repair a lost hand-off packet ahead of the first movement so the
            // receiver opens the session before the motion arrives.
            self.pair_enter_if_due(handle).await;
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
        self.send_unreliable(handle, event).await
    }

    async fn send_unreliable(&mut self, handle: CaptureHandle, event: ProtoEvent) -> bool {
        // Never wait for a (re)connection here: a motion/scroll event that is
        // a few hundred milliseconds old is useless, and blocking the capture
        // loop on it queued up hundreds of events that then arrived in one
        // jump. The capture is released after `grace` instead.
        match self.conn.send_now(event, handle).await {
            Ok(()) => {
                self.disconnected_since = None;
                true
            }
            Err(e) => {
                log::debug!("dropping high-rate input for client {handle}: {e}");
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
        let retry_interval = adaptive_enter_retry_interval(
            self.active_client.and_then(|h| self.conn.peer_rtt_ms(h)),
        );
        match self.enter.tick(Instant::now(), retry_interval) {
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
        let Some(pos) = self.get_pos(handle) else {
            // The capture was destroyed underneath us: nothing left to retry.
            return Ok(());
        };
        let pos = to_proto_pos(pos.opposite());
        let serial = self.enter.serial();
        let budget = self.grace.min(DISCRETE_SEND_BUDGET);
        if let Err(e) = self
            .conn
            .send_with_grace(ProtoEvent::Enter { pos, serial }, handle, budget)
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
        self.disconnected_since = None;
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
            } else {
                // Send a second `Leave(0)` datagram so a single lost UDP packet
                // on release does not leave the peer's remote-control state
                // latched until its watchdog fires.
                let _ = self.conn.send_now(ProtoEvent::Leave(0), handle).await;
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

/// Whether a capture event must be ignored because a remote peer is driving
/// this machine.
///
/// Only the incoming [`CaptureType::EnterOnly`] capture may react to a begin in
/// that state: it sends the `Leave` that ends the hand-off. A begin on an
/// outgoing capture comes from the peer's injected motion and would loop the
/// input back if it started a capture.
fn is_suppressed_remote_begin(
    remote_peer: Option<Position>,
    capture_type: CaptureType,
    event: &CaptureEvent,
) -> bool {
    remote_peer.is_some() && capture_type == CaptureType::Default && *event == CaptureEvent::Begin
}

/// Whether a `Leave` received while we hold the capture must release it.
///
/// `Leave(serial)` with a non-zero serial is an explicit return hand-off
/// request (retried until acknowledged). `Leave(0)` is the plain release
/// confirmation from peers that advertise
/// [`lan_mouse_proto::capabilities::SERIALIZED_LEAVE`]; older peers, including
/// builds that already acknowledge `Enter`, still use `Leave(0)` as the
/// request, so for them it releases. Without this distinction a confirmation
/// arriving right after a new capture started cancelled that capture and
/// bounced control between the machines at the shared edge.
fn leave_releases_capture(serial: u32, peer_supports_serialized_leave: bool) -> bool {
    serial != 0 || !peer_supports_serialized_leave
}

/// Whether `event` is a key or pointer-button release.
///
/// A lost release leaves the peer with the key/button latched until the next
/// event or the watchdog fires, so these are the only discrete events that are
/// transmitted twice (see the call site). Duplicate releases are harmless on a
/// deduplicating peer: keys are already filtered by the input-emulation
/// pressed-key set, and mouse buttons are filtered the same way.
fn is_release_event(event: &ProtoEvent) -> bool {
    matches!(
        event,
        ProtoEvent::Input(Event::Pointer(PointerEvent::Button { state: 0, .. }))
            | ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key { state: 0, .. }))
    )
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
    const RETRY: Duration = Duration::from_millis(35);

    #[test]
    fn begin_increments_the_serial() {
        let mut state = EnterState::default();
        let now = Instant::now();
        assert_eq!(state.serial(), 0);
        assert_eq!(state.tick(now, RETRY), EnterTick::Idle);

        state.begin(now, TIMEOUT);
        assert_eq!(state.serial(), 1);
        assert_eq!(state.awaiting_ack(), Some(1));
        assert_eq!(state.tick(now, RETRY), EnterTick::Idle);
        assert_eq!(state.tick(now + RETRY, RETRY), EnterTick::Retransmit);

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
        assert_eq!(state.tick(now + RETRY, RETRY), EnterTick::Idle);
    }

    #[test]
    fn tick_retransmits_after_interval_until_the_deadline_then_gives_up() {
        let mut state = EnterState::default();
        let now = Instant::now();
        state.begin(now, TIMEOUT);

        assert_eq!(
            state.tick(now + Duration::from_millis(10), RETRY),
            EnterTick::Idle
        );
        assert_eq!(
            state.tick(now + RETRY, RETRY),
            EnterTick::Retransmit,
            "due once retry interval elapsed"
        );
        assert_eq!(
            state.tick(now + RETRY + Duration::from_millis(10), RETRY),
            EnterTick::Idle,
            "not due again before another retry interval"
        );
        assert_eq!(
            state.tick(now + TIMEOUT, RETRY),
            EnterTick::GiveUp,
            "deadline is inclusive"
        );
        assert_eq!(state.tick(now + TIMEOUT * 2, RETRY), EnterTick::GiveUp);
    }

    #[test]
    fn adaptive_retry_interval_clamps_to_bounds() {
        assert_eq!(adaptive_enter_retry_interval(None), ENTER_RETRY_DEFAULT);
        assert_eq!(adaptive_enter_retry_interval(Some(2)), ENTER_RETRY_MIN);
        assert_eq!(
            adaptive_enter_retry_interval(Some(20)),
            Duration::from_millis(40)
        );
        assert_eq!(adaptive_enter_retry_interval(Some(80)), ENTER_RETRY_MAX);
    }

    #[test]
    fn reset_clears_the_pending_handoff() {
        let mut state = EnterState::default();
        let now = Instant::now();
        state.begin(now, TIMEOUT);
        state.reset();
        assert_eq!(state.awaiting_ack(), None);
        assert_eq!(state.tick(now + TIMEOUT * 2, RETRY), EnterTick::Idle);
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

    #[test]
    fn releases_are_recognised_for_redundant_transmission() {
        let button_release = ProtoEvent::Input(Event::Pointer(PointerEvent::Button {
            time: 0,
            button: input_event::BTN_LEFT,
            state: 0,
        }));
        let button_press = ProtoEvent::Input(Event::Pointer(PointerEvent::Button {
            time: 0,
            button: input_event::BTN_LEFT,
            state: 1,
        }));
        let key_release = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key: 30,
            state: 0,
        }));
        let key_press = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key: 30,
            state: 1,
        }));
        let motion = ProtoEvent::Input(Event::Pointer(PointerEvent::Motion {
            time: 0,
            dx: 1.0,
            dy: 0.0,
        }));

        assert!(is_release_event(&button_release));
        assert!(is_release_event(&key_release));
        assert!(!is_release_event(&button_press));
        assert!(!is_release_event(&key_press));
        assert!(!is_release_event(&motion));
        assert!(!is_release_event(&ProtoEvent::Leave(0)));
    }

    #[test]
    fn leave_release_requests_are_distinguished_from_confirmations() {
        assert!(
            leave_releases_capture(7, true),
            "a non-zero serial is an explicit return request and must release"
        );
        assert!(
            !leave_releases_capture(0, true),
            "Leave(0) from a serialized-leave peer is only a confirmation and must not cancel the new capture"
        );
        assert!(
            leave_releases_capture(0, false),
            "legacy peers use Leave(0) as the actual release request"
        );
        assert!(
            leave_releases_capture(7, false),
            "an unexpected serial from a legacy peer must not regress"
        );
    }

    #[test]
    fn a_remote_peer_only_allows_the_incoming_capture_to_begin() {
        let begin = CaptureEvent::Begin;
        let motion = CaptureEvent::Input(Event::Pointer(PointerEvent::Motion {
            time: 0,
            dx: 1.0,
            dy: 0.0,
        }));

        // No remote peer: everything behaves as before.
        assert!(!is_suppressed_remote_begin(
            None,
            CaptureType::Default,
            &begin
        ));
        // Remote peer driving us: the outgoing capture must not start.
        assert!(is_suppressed_remote_begin(
            Some(Position::Left),
            CaptureType::Default,
            &begin
        ));
        // The incoming capture still forwards the hand-off.
        assert!(!is_suppressed_remote_begin(
            Some(Position::Left),
            CaptureType::EnterOnly,
            &begin
        ));
        // Input events after a begin are not affected.
        assert!(!is_suppressed_remote_begin(
            Some(Position::Left),
            CaptureType::Default,
            &motion
        ));
    }
}

#[cfg(test)]
mod capture_handle_tests {
    use super::*;
    use crate::client::ClientManager;

    fn certificate() -> webrtc_dtls::crypto::Certificate {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join("lan-mouse-tests");
        std::fs::create_dir_all(&dir).expect("test dir");
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("capture-cert-{}-{unique}.pem", std::process::id()));
        let _ = std::fs::remove_file(&path);
        crate::crypto::generate_key_and_cert(&path).expect("generate certificate")
    }

    fn capture_task(captures: Vec<(CaptureHandle, Position, CaptureType)>) -> CaptureTask {
        let (event_tx, _event_rx) = channel();
        let (_request_tx, request_rx) = channel();
        CaptureTask {
            active_client: None,
            backend: Some(input_capture::Backend::Dummy),
            cancellation_token: CancellationToken::new(),
            captures,
            coalescer: MotionCoalescer::new(0),
            conn: LanMouseConnection::new(certificate(), ClientManager::default(), 1000),
            enter: EnterState::default(),
            last_enter_pair: None,
            sensitivity: HashMap::new(),
            outgoing_caps: HashMap::new(),
            remote_peer: None,
            grace: Duration::from_millis(1000),
            disconnected_since: None,
            event_tx,
            release_bind: Rc::new(RefCell::new(Vec::new())),
            request_rx,
            selected_backend: Default::default(),
            state: State::default(),
        }
    }

    /// The retry timer may be 25-100 ms away, so the first input after a lost
    /// `Enter` repairs the hand-off itself, at most once per throttle window.
    #[tokio::test(flavor = "current_thread")]
    async fn a_lost_enter_is_paired_at_most_once_per_throttle_window() {
        let mut task = capture_task(vec![(7, Position::Left, CaptureType::Default)]);
        task.active_client = Some(7);
        task.state = State::WaitingForAck;
        task.enter.begin(Instant::now(), Duration::from_secs(2));

        task.pair_enter_if_due(7).await;
        let first = task.last_enter_pair.expect("first pair is due immediately");
        task.pair_enter_if_due(7).await;
        assert_eq!(
            task.last_enter_pair,
            Some(first),
            "a second call inside the throttle window must not send another Enter"
        );

        // Simulate the throttle window elapsing instead of sleeping.
        task.last_enter_pair =
            Some(Instant::now() - ENTER_PAIR_THROTTLE - Duration::from_millis(1));
        task.pair_enter_if_due(7).await;
        assert!(
            task.last_enter_pair.is_some_and(|last| last > first),
            "the next input after the throttle window gets a fresh repair Enter"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pairing_is_inert_after_the_ack_and_for_other_handles() {
        let mut task = capture_task(vec![(7, Position::Left, CaptureType::Default)]);
        task.active_client = Some(7);
        task.enter.begin(Instant::now(), Duration::from_secs(2));
        task.state = State::Sending;
        task.pair_enter_if_due(7).await;
        assert!(
            task.last_enter_pair.is_none(),
            "an acknowledged hand-off must not be retransmitted"
        );

        task.state = State::WaitingForAck;
        task.pair_enter_if_due(8).await;
        assert!(
            task.last_enter_pair.is_none(),
            "only the active client's hand-off is repaired"
        );
    }

    #[test]
    fn lookups_of_a_destroyed_capture_return_none() {
        let task = capture_task(vec![(7, Position::Left, CaptureType::Default)]);
        assert_eq!(task.get_pos(7), Some(Position::Left));
        assert_eq!(task.get_type(7), Some(CaptureType::Default));
        assert_eq!(task.get_pos(8), None);
        assert_eq!(task.get_type(8), None);
    }

    /// Regression: an event queued for a capture that was destroyed in the
    /// meantime used to panic with "no such capture" and take the service down.
    /// Two captures can share a screen edge (Default + incoming EnterOnly), and
    /// InputCapture fans a backend event out to both ids, so a Destroy can
    /// overtake the queued copy.
    #[tokio::test(flavor = "current_thread")]
    async fn an_event_for_a_destroyed_capture_is_ignored() {
        let mut task = capture_task(vec![(7, Position::Left, CaptureType::Default)]);
        let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
            .await
            .expect("dummy capture");
        capture.create(7, Position::Left).await.expect("create");
        task.remove_capture(7);
        task.handle_capture_event(&mut capture, (7, CaptureEvent::Begin))
            .await
            .expect("a stale event must not fail or panic");
        capture.terminate().await.expect("terminate");
    }
}
