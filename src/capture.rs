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

use crate::handoff_trace;
use crate::{
    coalesce::MotionCoalescer,
    connect::{LanMouseConnection, LanMouseConnectionError},
};

/// Minimal connection surface used by the capture task.
///
/// Boxed so tests can drive the task with a deterministic fake instead of a
/// real DTLS/UDP connection; production uses [`LanMouseConnection`].
#[async_trait::async_trait(?Send)]
pub(crate) trait CaptureConnection {
    fn peer_supports_enter_ack(&self, handle: CaptureHandle) -> bool;
    fn peer_supports_serialized_leave(&self, handle: CaptureHandle) -> bool;
    fn peer_supports_edge_coordinate(&self, handle: CaptureHandle) -> bool;
    fn peer_rtt_ms(&self, handle: CaptureHandle) -> Option<u32>;
    fn is_connected(&self, handle: CaptureHandle) -> bool;
    fn ensure_connected(&self, handle: CaptureHandle);
    async fn send_with_grace(
        &self,
        event: ProtoEvent,
        handle: CaptureHandle,
        grace: Duration,
    ) -> Result<(), LanMouseConnectionError>;
    async fn send_now(
        &self,
        event: ProtoEvent,
        handle: CaptureHandle,
    ) -> Result<(), LanMouseConnectionError>;
    async fn recv(&mut self) -> (CaptureHandle, ProtoEvent);
    async fn disconnect(&self, handle: CaptureHandle);
    async fn drop_transport(&self, handle: CaptureHandle);
}

#[async_trait::async_trait(?Send)]
impl CaptureConnection for LanMouseConnection {
    fn peer_supports_enter_ack(&self, handle: CaptureHandle) -> bool {
        LanMouseConnection::peer_supports_enter_ack(self, handle)
    }

    fn peer_supports_serialized_leave(&self, handle: CaptureHandle) -> bool {
        LanMouseConnection::peer_supports_serialized_leave(self, handle)
    }

    fn peer_supports_edge_coordinate(&self, handle: CaptureHandle) -> bool {
        LanMouseConnection::peer_supports_edge_coordinate(self, handle)
    }

    fn peer_rtt_ms(&self, handle: CaptureHandle) -> Option<u32> {
        LanMouseConnection::peer_rtt_ms(self, handle)
    }

    fn is_connected(&self, handle: CaptureHandle) -> bool {
        LanMouseConnection::is_connected(self, handle)
    }

    fn ensure_connected(&self, handle: CaptureHandle) {
        LanMouseConnection::ensure_connected(self, handle);
    }

    async fn send_with_grace(
        &self,
        event: ProtoEvent,
        handle: CaptureHandle,
        grace: Duration,
    ) -> Result<(), LanMouseConnectionError> {
        LanMouseConnection::send_with_grace(self, event, handle, grace).await
    }

    async fn send_now(
        &self,
        event: ProtoEvent,
        handle: CaptureHandle,
    ) -> Result<(), LanMouseConnectionError> {
        LanMouseConnection::send_now(self, event, handle).await
    }

    async fn recv(&mut self) -> (CaptureHandle, ProtoEvent) {
        LanMouseConnection::recv(self).await
    }

    async fn disconnect(&self, handle: CaptureHandle) {
        LanMouseConnection::disconnect(self, handle).await;
    }

    async fn drop_transport(&self, handle: CaptureHandle) {
        LanMouseConnection::drop_transport(self, handle).await;
    }
}

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
            conn: Box::new(conn),
            enter: EnterState::default(),
            enter_along: None,
            residual_reverse_px: None,
            reverse_guard_until: None,
            handoff_inward_px: None,
            handoff_leading_until: None,
            handoff_begin_at: None,
            trace_window_until: None,
            trace_last_motion: None,
            last_enter_pair: None,
            sensitivity: Default::default(),
            remote_peer: None,
            remote_peer_since: None,
            enter_only_take_back_sent: false,
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
/// Nominal reverse-distance bound reported by the hand-off trace, in logical px.
///
/// Crossing a barrier is not instantaneous: the capture backend delivers the
/// compositor's pointer warp (back into the source screen) and the touchpad's
/// backswing before the motion that continues the crossing. On the receiver
/// those deltas point out of its entry edge, so forwarding them makes it hand
/// control straight back (enter/leave ping-pong). The observed activation warps
/// are not small: single deltas of ~63 px and backswing bursts totaling
/// ~35-50 px exist, and forwarding any of them used to look like a deliberate
/// hand-back. Suppression is therefore decided by the fixed
/// [`HANDOFF_REVERSE_GUARD`] window alone; this constant only seeds the
/// `remaining_px` field carried by `capture.reverse_motion_dropped` (it
/// saturates at 0 and no longer ends the guard).
const HANDOFF_RESIDUAL_MAX_PX: f64 = 32.0;
/// How long after `Begin` the hand-off suppresses reverse pointer motion.
///
/// The receiver is most sensitive to an outward push right after it entered: a
/// release 0.2.18 receiver parks the cursor on the entry edge and hands control
/// back as soon as the tracked pointer is pushed ~3 px past it. The touchpad's
/// backswing can arrive *after* the first forward delta, so a guard that stopped
/// at the first forward delta still leaked those residuals and kept the
/// enter/leave churn going. The window is fixed from `Begin` (it is not extended
/// by dropped deltas) so a held reverse gesture can never keep the filter alive,
/// and it drops *only* reverse deltas: forward motion is always forwarded
/// immediately, with no timed settle on all motion (which felt like the cursor
/// sticking at the edge). A genuine hand-back is delayed by at most this window,
/// never lost.
///
/// This constant stays the fixed-window part of the guard and its telemetry
/// deadline; the position-aware extension below is what decides when the guard
/// ends early (pointer clearly inward) or must keep running (late residual that
/// arrives after the fixed window while the pointer is still at the edge).
const HANDOFF_REVERSE_GUARD: Duration = Duration::from_millis(200);
/// Logical-px band next to the entry edge where reverse motion is still
/// activation residual, not a deliberate hand-back.
///
/// The source integrates only the deltas it actually forwarded, so this is the
/// best estimate of how far the receiver cursor has moved inside its screen.
/// While the tracked displacement is below this band the receiver cursor is
/// still parked next to its entry edge, where the residual (compositor warp /
/// touchpad backswing) is indistinguishable from a return push and immediately
/// hands control back - so reverse deltas stay suppressed. Once the pointer has
/// moved clearly inward the band is cleared for the rest of the hand-off and
/// reverse motion is user intent again.
///
/// The band is wider than the largest observed *leading forward* delta
/// (12.39 px) so the post-forward backswing is still suppressed, and small
/// enough that a user working on the remote screen clears it immediately.
const HANDOFF_ENTRY_BAND_PX: f64 = 16.0;
/// Upper bound of the position-aware guard, measured from `Begin`.
///
/// The bound is a safety escape, not the decision: it keeps a user who wants to
/// hand control back immediately from being blocked by a held near-edge reverse
/// gesture. Trace evidence: the first forwarded motion of a hand-off clusters
/// at 204.0-205.4 ms (30/95 samples), so the old fixed 200 ms window ended
/// ~4-5 ms before the hand-off actually settled; 250 ms covers that boundary
/// with margin while an intentional hand-back still passes at worst 250 ms
/// after `Begin`.
const HANDOFF_POSITION_GUARD: Duration = Duration::from_millis(250);
/// How long after a remote peer starts driving this machine an incoming
/// (`EnterOnly`) capture begin is treated as settling jitter instead of a local
/// take-back request.
///
/// While the peer drives, the injected absolute pointer is parked 1-2 px inside
/// the shared edge; every barrier re-activation there reports a `Begin` on the
/// incoming capture and used to become a `request_handoff(addr, None)` - the
/// observed `along=None` storm (up to 10 requests/s). A deliberate local
/// take-back after this short dwell is still reported (once per driving
/// session; the service retries a request until the peer confirms).
const REMOTE_ENTRY_DWELL: Duration = Duration::from_millis(300);
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

/// Signed component of a pointer motion that points out of the screen edge the
/// peer entered through, in logical px.
///
/// Positive values are the hand-off's activation residual (the compositor's
/// warp and the push's backswing); negative values are forward motion that
/// carries the remote cursor into its screen. `Position` is the local capture's
/// peer position (the side the pointer is leaving through), so the receiver
/// sees the opposite edge and the reverse direction is the crossing axis
/// reversed.
fn handoff_outward_px(pos: Position, dx: f64, dy: f64) -> f64 {
    match pos {
        // The peer sits left, so crossing motion is negative dx and reverse is
        // positive dx.
        Position::Left => dx,
        Position::Right => -dx,
        Position::Top => dy,
        Position::Bottom => -dy,
    }
}

/// Debounce a statement `$st`, i.e. the statement is executed only if the
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
    conn: Box<dyn CaptureConnection>,
    /// `Enter`/`Ack` handoff state
    enter: EnterState,
    /// Normalised crossing coordinate along the entry edge for the current
    /// hand-off (`BeginAt`); reused by retries and repair `EnterAt` events.
    enter_along: Option<f32>,
    /// Reverse-distance telemetry of the active hand-off guard, in logical px.
    /// `None` means no hand-off residual filter is active.
    ///
    /// The edge activation usually includes the compositor's pointer warp and
    /// the push's backswing, both pointing back into the source screen.
    /// Forwarding them makes the receiver see an outward motion at its entry
    /// edge and immediately hand control back (the capture enter/leave
    /// ping-pong). The filter drops reverse deltas while the tracked pointer is
    /// still inside the entry band (see [`Self::handoff_inward_px`]); observed
    /// warps exceed the nominal [`HANDOFF_RESIDUAL_MAX_PX`] in a single delta, so
    /// this value is trace telemetry only: it starts at the nominal bound and
    /// saturates at 0. The guard stays armed across forward deltas: old
    /// receivers still return on the backswing that follows the crossing
    /// motion.
    residual_reverse_px: Option<f64>,
    /// Deadline for the fixed-window part of the reverse-only hand-off guard;
    /// `None` while no guard is armed. Armed together with
    /// [`Self::residual_reverse_px`] on `Begin`. Kept as telemetry (the trace
    /// reports how much of the old fixed window is left); the actual decision
    /// is the position-aware pair below.
    reverse_guard_until: Option<Instant>,
    /// Tracked inward displacement (logical px) of the receiver cursor from the
    /// entry edge, integrated from the deltas the source actually forwarded.
    /// `Some` while the hand-off leading-residual filter is active, `None` once
    /// the pointer has clearly moved inward or the guard bounded out.
    ///
    /// Positive values are inside the remote screen; the receiver parks its
    /// absolute pointer on the entry edge, so while this stays below
    /// [`HANDOFF_ENTRY_BAND_PX`] a reverse delta is activation residual, not
    /// the user asking for the capture back.
    handoff_inward_px: Option<f64>,
    /// Upper bound of the position-aware part of the guard. Armed on `Begin`,
    /// cleared early once the pointer is clearly inward, or when the bound
    /// expires so a held reverse gesture can never be blocked indefinitely.
    handoff_leading_until: Option<Instant>,
    /// When the current hand-off began; used to log the first motion latency
    /// (debug only).
    handoff_begin_at: Option<Instant>,
    /// Opt-in trace: local capture-to-capture motion gaps during the first
    /// 500 ms after a Begin, to separate source cadence from network jitter.
    trace_window_until: Option<Instant>,
    trace_last_motion: Option<Instant>,
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
    /// When the remote peer started driving this machine (edge change or
    /// `None` -> `Some`), used for the incoming-capture take-back dwell.
    remote_peer_since: Option<Instant>,
    /// Whether a local take-back request was already surfaced for the current
    /// remote-driving session. The service retries a `request_handoff` until
    /// the peer confirms, so one request per session is enough; repeated
    /// incoming-capture begins are the `along=None` storm.
    enter_only_take_back_sent: bool,
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

    /// Update the edge a remote peer drives this machine from.
    ///
    /// A change of edge (including `None`<->`Some`) starts a new
    /// remote-driving session: the settling dwell restarts and a new local
    /// take-back request is allowed once. Re-sending the same edge (the
    /// service repeats it for retransmitted `Enter`s) keeps the state, so the
    /// one-request-per-session gate cannot be reset by a duplicate `Enter`.
    fn update_remote_peer(&mut self, peer: Option<lan_mouse_ipc::Position>) {
        if self.remote_peer == peer {
            return;
        }
        log::debug!("remote peer driving this machine changed: {peer:?}");
        self.remote_peer = peer;
        self.remote_peer_since = peer.map(|_| Instant::now());
        self.enter_only_take_back_sent = false;
    }

    /// Whether an incoming (`EnterOnly`) capture begin should be surfaced as a
    /// local take-back request.
    ///
    /// While a remote peer drives this machine, its injected absolute pointer
    /// is parked right at the shared edge; every barrier re-activation there
    /// (activation residual, compositor warp, injected jitter) reports a
    /// `Begin` on the incoming capture. Surfacing all of them made the service
    /// send a `request_handoff(addr, None)` per activation - the observed
    /// `along=None` storm (up to 10 per second), each release re-arming the
    /// barrier for the next one. The service retries an unconfirmed take-back
    /// request on its own, so a single request per remote-driving session is
    /// enough:
    ///
    /// * begins during [`REMOTE_ENTRY_DWELL`] after the peer started driving are
    ///   settling jitter and are ignored;
    /// * further begins in the same session are ignored (already requested);
    /// * after the dwell the first genuine local take-back is still reported,
    ///   and a new remote-driving session re-arms the gate.
    fn should_report_enter_only_begin(&mut self, capture_type: CaptureType) -> bool {
        if capture_type != CaptureType::EnterOnly || self.remote_peer.is_none() {
            return true;
        }
        if self.enter_only_take_back_sent {
            log::debug!(
                "ignoring repeated incoming-capture begin: take-back already requested for this remote session"
            );
            return false;
        }
        if self
            .remote_peer_since
            .is_some_and(|since| since.elapsed() < REMOTE_ENTRY_DWELL)
        {
            log::debug!("ignoring incoming-capture begin during the remote-entry dwell");
            return false;
        }
        self.enter_only_take_back_sent = true;
        true
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
                            self.update_remote_peer(peer);
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
                        self.release_capture(capture, None).await?;
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
                            self.release_capture(capture, None).await?;
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
                                    self.release_capture(capture, None).await?;
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
                                    self.release_capture(capture, None).await?;
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
                        ProtoEvent::LeaveAt { serial, along } => {
                            if self.active_client == Some(handle) {
                                log::info!(
                                    "releasing capture at return coordinate {along:.3} (LeaveAt({serial}))"
                                );
                                self.release_capture(capture, Some(along)).await?;
                            } else if serial != 0 {
                                log::debug!(
                                    "re-confirming capture release to {handle} (retried LeaveAt({serial}))"
                                );
                                if let Err(e) =
                                    self.conn.send_now(ProtoEvent::Leave(0), handle).await
                                {
                                    log::debug!("failed to re-confirm release: {e}");
                                }
                            }
                        }
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
                        self.update_remote_peer(peer);
                        capture.set_remote_peer(peer.map(to_capture_pos));
                    }
                    CaptureRequest::Reenable => { /* already active */ },
                    CaptureRequest::Release => self.release_capture(capture, None).await?,
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
                            self.release_capture(capture, None).await?;
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
                        if let Err(e) = self.release_capture(capture, None).await {
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
            CaptureEvent::Begin | CaptureEvent::BeginAt(_) => {
                caps.intersects(Capabilities::POINTER) || caps.intersects(Capabilities::KEYBOARD)
            }
            CaptureEvent::Input(Event::Pointer(_)) => caps.contains(Capabilities::POINTER),
            CaptureEvent::Input(Event::Keyboard(_)) => caps.contains(Capabilities::KEYBOARD),
        }
    }

    /// Build the hand-off event for `handle`.
    ///
    /// `EnterAt` is only used when the peer advertised
    /// [`lan_mouse_proto::capabilities::EDGE_COORDINATE`] and the capture
    /// backend reported where the pointer crossed; otherwise the legacy
    /// `Enter` keeps the old edge-centre fallback.
    fn enter_event(
        &self,
        handle: CaptureHandle,
        opposite_pos: lan_mouse_proto::Position,
    ) -> ProtoEvent {
        match self.enter_along {
            Some(along) if self.conn.peer_supports_edge_coordinate(handle) => ProtoEvent::EnterAt {
                pos: opposite_pos,
                serial: self.enter.serial(),
                along,
            },
            _ => ProtoEvent::Enter {
                pos: opposite_pos,
                serial: self.enter.serial(),
            },
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
            return self.release_capture(capture, None).await;
        }

        if !self.outgoing_event_allowed(handle, &event) {
            log::debug!(
                "dropping {event:?} for client {handle}: capability not granted for the outgoing direction"
            );
            return Ok(());
        }

        // Surface the begin as a local take-back request only when it can be
        // one. While a remote peer drives this machine, the injected pointer is
        // parked on the shared edge and every barrier re-activation reports an
        // incoming-capture begin; those must not turn into a `Leave` per
        // activation (the signature of the reported `along=None` storm).
        if event.is_begin() && self.should_report_enter_only_begin(capture_type) {
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
        if event.is_begin() && Some(handle) != self.active_client {
            // The peer is unreachable (e.g. the user clicked Connect while the
            // other machine is off). The backend has already grabbed/locked
            // the local pointer for this begin, but without a transport the
            // `Enter` cannot be delivered; taking the capture would hold the
            // pointer at the edge until the retry timeout and fight the
            // compositor on release. Give the backend back immediately and
            // retry only on a fresh crossing once connected.
            if !self.conn.is_connected(handle) {
                log::info!(
                    "releasing capture before grabbing: client {handle} has no established connection"
                );
                self.active_client = None;
                self.state = State::WaitingForAck;
                self.enter.reset();
                self.disconnected_since = None;
                capture.release().await?;
                return Ok(());
            }
            self.active_client.replace(handle);
            self.enter_along = event.begin_along();
            let handoff_began = Instant::now();
            self.residual_reverse_px = Some(HANDOFF_RESIDUAL_MAX_PX);
            self.reverse_guard_until = Some(handoff_began + HANDOFF_REVERSE_GUARD);
            self.handoff_inward_px = Some(0.0);
            self.handoff_leading_until = Some(handoff_began + HANDOFF_POSITION_GUARD);
            self.handoff_begin_at = Some(handoff_began);
            if handoff_trace::enabled() {
                self.trace_window_until = Some(Instant::now() + Duration::from_millis(500));
                self.trace_last_motion = None;
            }
            handoff_trace::event(
                "controller",
                "capture.begin",
                None,
                Some(&format!("client {handle}")),
                &[
                    ("pos", format!("{:?}", self.get_pos(handle))),
                    ("along", format!("{:?}", self.enter_along)),
                ],
            );
            // The first motion of the hand-off must not wait for a batch
            // window inherited from the movement on the previous screen.
            self.coalescer.arm_leading_edge();
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

        // Crossing an edge is not instantaneous: right after `Begin` the
        // capture backend delivers the compositor's pointer warp and the
        // touchpad's backswing, both pointing back into the source screen. On
        // the receiver they are an outward push at its entry edge and hand
        // control straight back (the enter/leave ping-pong).
        //
        // The decision is position-aware: the source integrates the deltas it
        // actually forwarded into [`Self::handoff_inward_px`], the estimated
        // inward displacement of the receiver cursor from the entry edge. While
        // that estimate is below [`HANDOFF_ENTRY_BAND_PX`] the cursor is still
        // parked next to the edge, where an outward push is indistinguishable
        // from activation residual (observed warps reach 63 px in a single
        // delta), so reverse deltas are dropped. Once the pointer has clearly
        // moved inside, reverse motion is the user's deliberate hand-back and
        // is forwarded immediately - even before any timer expired.
        //
        // The bounded filter ([`HANDOFF_POSITION_GUARD`]) keeps a held reverse
        // gesture from being blocked: an intentional hand-back passes at worst
        // 250 ms after `Begin`, never lost. The fixed
        // [`HANDOFF_REVERSE_GUARD`] window stays armed next to it as the
        // bounded telemetry deadline that the trace reports.
        //
        // Forward deltas are never delayed or dropped by the filter (a timed
        // settle on all motion felt like the cursor sticking at the edge): they
        // are the crossing motion that moves the receiver cursor inward.
        //
        // The window is the only decision in the superseded guard; it is kept
        // here as that telemetry deadline only. Suppression telemetry
        // (`remaining_px`) is still seeded with [`HANDOFF_RESIDUAL_MAX_PX`] and
        // saturates at 0: dropped deltas never extend the filter.
        if let Some(inward_px) = self.handoff_inward_px {
            let now = Instant::now();
            let fixed_window = self.reverse_guard_until.is_some_and(|until| now < until);
            let position_guard = self.handoff_leading_until.is_some_and(|until| now < until);
            let near_edge = inward_px < HANDOFF_ENTRY_BAND_PX;
            if near_edge && (fixed_window || position_guard) {
                if let CaptureEvent::Input(Event::Pointer(PointerEvent::Motion {
                    dx, dy, ..
                })) = event
                {
                    let reverse_px = self
                        .get_pos(handle)
                        .map(|pos| handoff_outward_px(pos, dx, dy))
                        .unwrap_or(0.0);
                    if reverse_px > 0.0 {
                        // Reverse residual while the receiver cursor is still
                        // next to its entry edge: suppress the whole delta,
                        // however large. The remaining distance is trace
                        // telemetry only and saturates at 0.
                        let remaining_px = self.residual_reverse_px.unwrap_or(0.0);
                        let left_px = (remaining_px - reverse_px).max(0.0);
                        self.residual_reverse_px = Some(left_px);
                        // The filter started when the tracked displacement was
                        // armed (`Begin`); report progress for the trace.
                        let begin = self
                            .handoff_leading_until
                            .and_then(|until| until.checked_sub(HANDOFF_POSITION_GUARD))
                            .or_else(|| {
                                self.reverse_guard_until
                                    .and_then(|until| until.checked_sub(HANDOFF_REVERSE_GUARD))
                            })
                            .unwrap_or(now);
                        let guard_remaining_us = self
                            .handoff_leading_until
                            .map(|until| until.saturating_duration_since(now).as_micros())
                            .unwrap_or_default();
                        handoff_trace::event(
                            "controller",
                            "capture.reverse_motion_dropped",
                            Some(self.enter.serial()),
                            Some(&format!("client {handle}")),
                            &[
                                ("dx", dx.to_string()),
                                ("dy", dy.to_string()),
                                ("remaining_px", left_px.to_string()),
                                (
                                    "inward_px",
                                    self.handoff_inward_px.unwrap_or(inward_px).to_string(),
                                ),
                                (
                                    "elapsed_us",
                                    now.saturating_duration_since(begin).as_micros().to_string(),
                                ),
                                ("guard_remaining_us", guard_remaining_us.to_string()),
                                (
                                    "window_remaining_us",
                                    self.reverse_guard_until
                                        .map(|until| {
                                            until.saturating_duration_since(now).as_micros()
                                        })
                                        .unwrap_or_default()
                                        .to_string(),
                                ),
                            ],
                        );
                        return Ok(());
                    }
                    // Forward motion is always forwarded immediately; integrate
                    // it into the tracked inward displacement. Lateral motion
                    // does not change the distance from the edge.
                    self.handoff_inward_px = Some(inward_px + (-reverse_px).max(0.0));
                }
            } else {
                // Hand-off settled: the pointer is clearly inward (or the
                // bounded filter expired). Reverse motion passes from now on.
                self.handoff_inward_px = None;
                self.handoff_leading_until = None;
                self.residual_reverse_px = None;
                self.reverse_guard_until = None;
            }
        }

        // Coalesce pointer motion while a client is active. Motion that
        // arrives while we are still waiting for `Ack` is buffered and flushed
        // on the normal cadence (a lost `Enter` is repaired ahead of it by
        // `pair_enter_if_due`), so the handshake window does not drop the first
        // movements.
        if self.coalescer.enabled() && self.active_client == Some(handle) {
            if let CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { time, dx, dy })) =
                event
            {
                if handoff_trace::enabled()
                    && self
                        .trace_window_until
                        .is_some_and(|until| Instant::now() < until)
                {
                    let now = Instant::now();
                    if let Some(last) = self.trace_last_motion {
                        let gap = now.saturating_duration_since(last);
                        if gap > handoff_trace::INPUT_GAP_WARN {
                            handoff_trace::event(
                                "controller",
                                "controller.input_gap_exceeded",
                                Some(self.enter.serial()),
                                Some(&format!("client {handle}")),
                                &[("gap_us", gap.as_micros().to_string())],
                            );
                        }
                    }
                    self.trace_last_motion = Some(now);
                }
                let due = self.coalescer.push(handle, time, dx, dy, Instant::now());
                if due && !self.flush_motion().await {
                    log::warn!("releasing capture: failed to send coalesced motion");
                    return self.release_capture(capture, None).await;
                }
                return Ok(());
            }
        }

        // Preserve ordering: never let a discrete event overtake pending
        // motion. `Begin` is excluded because it is the *start* of a new
        // session and any pending motion belongs to the old one.
        if self.active_client == Some(handle)
            && !event.is_begin()
            && !self.flush_handle(handle).await
        {
            return self.release_capture(capture, None).await;
        }

        let Some(pos) = self.get_pos(handle) else {
            return Ok(());
        };
        let opposite_pos = to_proto_pos(pos.opposite());
        let is_begin = event.is_begin();

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
            CaptureEvent::Begin | CaptureEvent::BeginAt(_) => {
                let event = self.enter_event(handle, opposite_pos);
                handoff_trace::event(
                    "controller",
                    "capture.enter_queued",
                    Some(self.enter.serial()),
                    Some(&format!("client {handle}")),
                    &[(
                        "elapsed_us",
                        self.handoff_begin_at
                            .map(|begin| begin.elapsed().as_micros())
                            .unwrap_or_default()
                            .to_string(),
                    )],
                );
                event
            }
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
                if is_begin {
                    handoff_trace::event(
                        "controller",
                        "capture.enter_sent",
                        Some(self.enter.serial()),
                        Some(&format!("client {handle}")),
                        &[(
                            "elapsed_us",
                            self.handoff_begin_at
                                .map(|begin| begin.elapsed().as_micros())
                                .unwrap_or_default()
                                .to_string(),
                        )],
                    );
                }
                self.disconnected_since = None;
                // A single lost `Enter` stalls the hand-off until the retry
                // timer. Sending a second copy on the same path mirrors the
                // redundant `Leave(0)` and makes the common case survive an
                // isolated packet loss; the receiver re-acks instead of
                // re-applying it.
                if is_begin && self.state == State::WaitingForAck {
                    let repair = self.enter_event(handle, opposite_pos);
                    let _ = self.conn.send_now(repair, handle).await;
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
                    self.release_capture(capture, None).await?;
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
        let event = self.enter_event(handle, to_proto_pos(pos.opposite()));
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
        let first_after_begin = self.handoff_begin_at.take();
        let sent = self.send_unreliable(handle, event).await;
        if let Some(begin) = first_after_begin {
            let elapsed = begin.elapsed();
            log::debug!(
                "handoff: Begin -> first motion sent in {} us",
                elapsed.as_micros()
            );
            handoff_trace::event(
                "controller",
                "capture.first_motion_sent",
                Some(self.enter.serial()),
                Some(&format!("client {handle}")),
                &[
                    ("elapsed_us", elapsed.as_micros().to_string()),
                    ("dx", motion.dx.to_string()),
                    ("dy", motion.dy.to_string()),
                ],
            );
        }
        sent
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
                return self.release_capture(capture, None).await;
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
        let event = self.enter_event(handle, pos);
        let budget = self.grace.min(DISCRETE_SEND_BUDGET);
        if let Err(e) = self.conn.send_with_grace(event, handle, budget).await {
            log::debug!("failed to retransmit Enter to client {handle}: {e}");
        }
        Ok(())
    }

    async fn release_capture(
        &mut self,
        capture: &mut InputCapture,
        return_along: Option<f32>,
    ) -> Result<(), CaptureError> {
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
        // The hand-off is over: drop the leading-residual filter state so a
        // later session starts from a clean estimate.
        self.residual_reverse_px = None;
        self.reverse_guard_until = None;
        self.handoff_inward_px = None;
        self.handoff_leading_until = None;

        // Remember where this capture lives before clearing it: the peer's
        // return coordinate is relative to that edge.
        let active_pos = self.active_client.and_then(|handle| self.get_pos(handle));
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
        if let (Some(pos), Some(along)) = (active_pos, return_along) {
            capture.release_at(pos, along).await
        } else {
            capture.release().await
        }
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
    remote_peer.is_some() && capture_type == CaptureType::Default && event.is_begin()
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
        // Default tests use the deterministic fake connection; the real
        // LanMouseConnection is only needed when the test exercises peer
        // capability negotiation.
        capture_task_with_connection(captures, Box::new(FakeConnection::new()))
    }

    fn capture_task_with_manager(
        captures: Vec<(CaptureHandle, Position, CaptureType)>,
        manager: ClientManager,
    ) -> CaptureTask {
        capture_task_with_connection(
            captures,
            Box::new(LanMouseConnection::new(certificate(), manager, 1000)),
        )
    }

    fn capture_task_with_connection(
        captures: Vec<(CaptureHandle, Position, CaptureType)>,
        conn: Box<dyn CaptureConnection>,
    ) -> CaptureTask {
        capture_task_with_events(captures, conn).0
    }

    /// Like [`Self::capture_task`] but keeps the event receiver, so tests can
    /// observe which `ICaptureEvent`s the task surfaces.
    fn capture_task_with_events(
        captures: Vec<(CaptureHandle, Position, CaptureType)>,
        conn: Box<dyn CaptureConnection>,
    ) -> (CaptureTask, Receiver<ICaptureEvent>) {
        let (event_tx, event_rx) = channel();
        let (_request_tx, request_rx) = channel();
        let task = CaptureTask {
            active_client: None,
            backend: Some(input_capture::Backend::Dummy),
            cancellation_token: CancellationToken::new(),
            captures,
            coalescer: MotionCoalescer::new(0),
            conn,
            enter: EnterState::default(),
            enter_along: None,
            residual_reverse_px: None,
            reverse_guard_until: None,
            handoff_inward_px: None,
            handoff_leading_until: None,
            handoff_begin_at: None,
            trace_window_until: None,
            trace_last_motion: None,
            last_enter_pair: None,
            sensitivity: HashMap::new(),
            outgoing_caps: HashMap::new(),
            remote_peer: None,
            remote_peer_since: None,
            enter_only_take_back_sent: false,
            grace: Duration::from_millis(1000),
            disconnected_since: None,
            event_tx,
            release_bind: Rc::new(RefCell::new(Vec::new())),
            request_rx,
            selected_backend: Default::default(),
            state: State::default(),
        };
        (task, event_rx)
    }

    #[test]
    fn enter_event_uses_enter_at_only_when_the_peer_advertises_it() {
        use crate::config::ConfigClient;
        use lan_mouse_proto::{CAPABILITIES, Position as ProtoPosition};

        let config = |active: bool| ConfigClient {
            ips: std::collections::HashSet::new(),
            hostname: None,
            port: 4242,
            pos: lan_mouse_ipc::Position::Left,
            active,
            enter_hook: None,
            leave_hook: None,
            sensitivity: 100,
            fingerprint: None,
        };

        let modern = ClientManager::default();
        let modern_handle = modern.add_with_config(config(true));
        modern.set_peer_hello(modern_handle, Some(*b"fork0000"), 1, CAPABILITIES, None);
        let mut task = capture_task_with_manager(vec![], modern);
        task.enter_along = Some(0.25);
        task.enter.begin(Instant::now(), Duration::from_secs(2));
        match task.enter_event(modern_handle, ProtoPosition::Right) {
            ProtoEvent::EnterAt { pos, along, .. } => {
                assert_eq!(pos as u8, ProtoPosition::Right as u8);
                assert_eq!(along, 0.25);
            }
            other => panic!("modern peer must get EnterAt, got {other:?}"),
        }

        let legacy = ClientManager::default();
        let legacy_handle = legacy.add_with_config(config(true));
        legacy.set_peer_hello(legacy_handle, Some(*b"legacy00"), 0, 0, None);
        let mut task = capture_task_with_manager(vec![], legacy);
        task.enter_along = Some(0.25);
        task.enter.begin(Instant::now(), Duration::from_secs(2));
        assert!(matches!(
            task.enter_event(legacy_handle, ProtoPosition::Right),
            ProtoEvent::Enter { .. }
        ));
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

    #[derive(Clone)]
    struct FakeConnection {
        sent: Rc<RefCell<Vec<ProtoEvent>>>,
        supports_ack: bool,
        supports_serialized_leave: bool,
        supports_edge_coordinate: bool,
        connected: bool,
    }

    impl FakeConnection {
        fn new() -> Self {
            Self {
                sent: Rc::new(RefCell::new(Vec::new())),
                supports_ack: true,
                supports_serialized_leave: true,
                supports_edge_coordinate: true,
                connected: true,
            }
        }
    }

    #[async_trait::async_trait(?Send)]
    impl CaptureConnection for FakeConnection {
        fn peer_supports_enter_ack(&self, _handle: CaptureHandle) -> bool {
            self.supports_ack
        }

        fn peer_supports_serialized_leave(&self, _handle: CaptureHandle) -> bool {
            self.supports_serialized_leave
        }

        fn peer_supports_edge_coordinate(&self, _handle: CaptureHandle) -> bool {
            self.supports_edge_coordinate
        }

        fn peer_rtt_ms(&self, _handle: CaptureHandle) -> Option<u32> {
            Some(2)
        }

        fn is_connected(&self, _handle: CaptureHandle) -> bool {
            self.connected
        }

        fn ensure_connected(&self, _handle: CaptureHandle) {}

        async fn send_with_grace(
            &self,
            event: ProtoEvent,
            _handle: CaptureHandle,
            _grace: Duration,
        ) -> Result<(), LanMouseConnectionError> {
            if !self.connected {
                return Err(LanMouseConnectionError::NotConnected);
            }
            self.sent.borrow_mut().push(event);
            Ok(())
        }

        async fn send_now(
            &self,
            event: ProtoEvent,
            _handle: CaptureHandle,
        ) -> Result<(), LanMouseConnectionError> {
            if !self.connected {
                return Err(LanMouseConnectionError::NotConnected);
            }
            self.sent.borrow_mut().push(event);
            Ok(())
        }

        async fn recv(&mut self) -> (CaptureHandle, ProtoEvent) {
            std::future::pending::<()>().await;
            unreachable!()
        }

        async fn disconnect(&self, _handle: CaptureHandle) {}

        async fn drop_transport(&self, _handle: CaptureHandle) {}
    }

    #[tokio::test(flavor = "current_thread")]
    async fn begin_at_is_forwarded_as_enter_at_with_the_crossing_coordinate() {
        let fake = FakeConnection::new();
        let sent = fake.sent.clone();
        let mut task = capture_task_with_connection(
            vec![(7, Position::Left, CaptureType::Default)],
            Box::new(fake),
        );
        let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
            .await
            .expect("dummy capture");

        task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.25)))
            .await
            .expect("begin");
        {
            let sent = sent.borrow();
            match sent.first() {
                Some(ProtoEvent::EnterAt { pos, along, .. }) => {
                    assert_eq!(*pos as u8, lan_mouse_proto::Position::Right as u8);
                    assert_eq!(*along, 0.25);
                }
                other => panic!("expected EnterAt, got {other:?}"),
            }
        }

        task.handle_capture_event(
            &mut capture,
            (
                7,
                CaptureEvent::Input(Event::Pointer(PointerEvent::Motion {
                    time: 0,
                    dx: -2.0,
                    dy: 0.0,
                })),
            ),
        )
        .await
        .expect("motion");
        {
            let sent = sent.borrow();
            assert!(
                sent.iter()
                    .any(|event| matches!(event, ProtoEvent::Input(_))),
                "forward motion must be forwarded after EnterAt: {sent:?}"
            );
        }

        capture.terminate().await.expect("terminate");
    }

    /// The crossing motion must reach the peer at once. A timed settle window
    /// here was felt as the cursor briefly sticking at the shared edge, and it
    /// also made the user pull back (reverse motion) to retry.
    #[tokio::test(flavor = "current_thread")]
    async fn the_first_forward_motion_after_a_handoff_is_forwarded_immediately() {
        let fake = FakeConnection::new();
        let sent = fake.sent.clone();
        let mut task = capture_task_with_connection(
            vec![(7, Position::Left, CaptureType::Default)],
            Box::new(fake),
        );
        let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
            .await
            .expect("dummy capture");
        task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
            .await
            .expect("begin");
        // Keep the guard window wide so the assertion does not race test
        // scheduling; the real 200 ms value is pinned in a separate test.
        task.reverse_guard_until = Some(Instant::now() + Duration::from_secs(60));

        send_pointer(&mut task, &mut capture, -2.0, 0.0).await;

        assert_eq!(
            sent_motions(&sent),
            vec![(-2.0, 0.0)],
            "the first forward motion must be forwarded without any settle window"
        );
        assert_eq!(
            task.residual_reverse_px,
            Some(HANDOFF_RESIDUAL_MAX_PX),
            "a forward delta must not disarm the reverse guard: an old receiver \
             returns on the backswing that arrives after the crossing motion"
        );
        capture.terminate().await.expect("terminate");
    }

    /// Crossing an edge produces a burst of reverse deltas (compositor warp and
    /// backswing) before the motion that continues the crossing. Forwarding
    /// them makes the receiver hand control straight back, so the whole leading
    /// burst is suppressed.
    #[tokio::test(flavor = "current_thread")]
    async fn the_leading_reverse_burst_after_a_handoff_is_suppressed() {
        let fake = FakeConnection::new();
        let sent = fake.sent.clone();
        let mut task = capture_task_with_connection(
            vec![(7, Position::Left, CaptureType::Default)],
            Box::new(fake),
        );
        let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
            .await
            .expect("dummy capture");
        task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
            .await
            .expect("begin");
        task.reverse_guard_until = Some(Instant::now() + Duration::from_secs(60));

        // Back into the source screen: on the receiver these point out of its
        // entry edge and trigger an immediate LeaveAt.
        for dx in [2.0, 5.0, 7.0] {
            send_pointer(&mut task, &mut capture, dx, 0.0).await;
        }
        assert!(
            sent_motions(&sent).is_empty(),
            "the residual reverse burst must not be forwarded: {:?}",
            sent.borrow()
        );
        assert_eq!(
            task.residual_reverse_px,
            Some(HANDOFF_RESIDUAL_MAX_PX - 14.0),
            "the suppressed distance must be tracked"
        );

        // The actual crossing motion is forwarded.
        send_pointer(&mut task, &mut capture, -2.0, 0.0).await;
        assert_eq!(
            sent_motions(&sent),
            vec![(-2.0, 0.0)],
            "forward motion after the residual burst must be forwarded"
        );
        assert_eq!(
            task.residual_reverse_px,
            Some(HANDOFF_RESIDUAL_MAX_PX - 14.0),
            "forward motion neither consumes the reverse budget nor disarms the guard"
        );
        capture.terminate().await.expect("terminate");
    }

    /// Simulate the bounded position-aware hand-off filter expiring without
    /// sleeping: the pointer never moved clearly inward.
    fn expire_handoff_filter(task: &mut CaptureTask) {
        let expired = Instant::now() - Duration::from_millis(1);
        task.reverse_guard_until = Some(expired);
        task.handoff_leading_until = Some(expired);
    }

    /// The touchpad backswing keeps producing reverse deltas *after* the
    /// crossing motion. Old receivers hand control back on those residuals, so
    /// the guard must stay armed after the first forward delta: the tracked
    /// pointer is still inside the entry band while the backswing arrives.
    #[tokio::test(flavor = "current_thread")]
    async fn reverse_residual_after_the_first_forward_is_suppressed_until_the_handoff_settles() {
        let fake = FakeConnection::new();
        let sent = fake.sent.clone();
        let mut task = capture_task_with_connection(
            vec![(7, Position::Left, CaptureType::Default)],
            Box::new(fake),
        );
        let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
            .await
            .expect("dummy capture");
        task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
            .await
            .expect("begin");
        let pinned = Instant::now() + Duration::from_secs(60);
        task.reverse_guard_until = Some(pinned);
        let pinned_leading = Instant::now() + Duration::from_secs(60);
        task.handoff_leading_until = Some(pinned_leading);

        // The crossing motion is forwarded at once.
        send_pointer(&mut task, &mut capture, -3.0, 0.0).await;
        // The post-forward backswing residuals are dropped.
        send_pointer(&mut task, &mut capture, 4.0, 0.0).await;
        send_pointer(&mut task, &mut capture, 6.0, 0.0).await;

        assert_eq!(
            sent_motions(&sent),
            vec![(-3.0, 0.0)],
            "reverse residuals after the first forward delta must stay suppressed"
        );
        assert_eq!(
            task.residual_reverse_px,
            Some(HANDOFF_RESIDUAL_MAX_PX - 10.0),
            "the post-forward residual distance must be tracked"
        );
        assert_eq!(
            task.reverse_guard_until,
            Some(pinned),
            "dropped deltas must not extend the fixed hand-off window"
        );
        assert_eq!(
            task.handoff_leading_until,
            Some(pinned_leading),
            "dropped deltas must not extend the position-aware bound either"
        );

        // Once the bounded filter is over the same reverse motion is forwarded
        // (the deliberate hand-back is delayed, never lost).
        expire_handoff_filter(&mut task);
        send_pointer(&mut task, &mut capture, 5.0, 0.0).await;
        assert_eq!(sent_motions(&sent), vec![(-3.0, 0.0), (5.0, 0.0)]);
        assert!(task.residual_reverse_px.is_none());
        assert!(task.reverse_guard_until.is_none());
        assert!(task.handoff_inward_px.is_none());
        assert!(task.handoff_leading_until.is_none());
        capture.terminate().await.expect("terminate");
    }

    /// No timed settle: the guard is direction-aware. Forward deltas are
    /// forwarded immediately even while reverse residuals are being dropped.
    #[tokio::test(flavor = "current_thread")]
    async fn forward_motion_is_never_delayed_or_dropped_by_the_reverse_guard() {
        let fake = FakeConnection::new();
        let sent = fake.sent.clone();
        let mut task = capture_task_with_connection(
            vec![(7, Position::Left, CaptureType::Default)],
            Box::new(fake),
        );
        let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
            .await
            .expect("dummy capture");
        task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
            .await
            .expect("begin");
        task.reverse_guard_until = Some(Instant::now() + Duration::from_secs(60));

        send_pointer(&mut task, &mut capture, 4.0, 0.0).await;
        send_pointer(&mut task, &mut capture, -2.0, 0.0).await;
        send_pointer(&mut task, &mut capture, -3.0, 0.0).await;
        send_pointer(&mut task, &mut capture, 5.0, 0.0).await;

        assert_eq!(
            sent_motions(&sent),
            vec![(-2.0, 0.0), (-3.0, 0.0)],
            "forward deltas must be forwarded immediately, reverse deltas dropped"
        );
        assert_eq!(
            task.residual_reverse_px,
            Some(HANDOFF_RESIDUAL_MAX_PX - 9.0)
        );
        capture.terminate().await.expect("terminate");
    }

    /// A single observed activation warp can exceed the old 32 px accumulation
    /// bound by itself (serial 3: -63.272 px). Forwarding it whole was
    /// interpreted by the old receiver as a deliberate hand-back and fired
    /// `LeaveAt`, keeping the source cursor visible at the border.
    #[tokio::test(flavor = "current_thread")]
    async fn a_single_large_reverse_warp_inside_the_window_is_suppressed() {
        let fake = FakeConnection::new();
        let sent = fake.sent.clone();
        let mut task = capture_task_with_connection(
            vec![(7, Position::Left, CaptureType::Default)],
            Box::new(fake),
        );
        let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
            .await
            .expect("dummy capture");
        task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
            .await
            .expect("begin");
        task.reverse_guard_until = Some(Instant::now() + Duration::from_secs(60));

        send_pointer(&mut task, &mut capture, 63.272, 0.0).await;
        assert!(
            sent_motions(&sent).is_empty(),
            "a single large activation warp must be suppressed inside the window"
        );
        assert_eq!(
            task.residual_reverse_px,
            Some(0.0),
            "the trace distance saturates at 0 but the window keeps the guard armed"
        );

        // Forward motion crosses immediately, and reverse motion after it is
        // still inside the same fixed window.
        send_pointer(&mut task, &mut capture, -3.025, 0.0).await;
        send_pointer(&mut task, &mut capture, 17.961, 0.0).await;
        assert_eq!(
            sent_motions(&sent),
            vec![(-3.025, 0.0)],
            "forward motion is immediate; the post-forward residual is guarded"
        );
        capture.terminate().await.expect("terminate");
    }

    /// A reverse burst larger than the old 32 px accumulation bound used to be
    /// forwarded whole as a "deliberate hand-back". The fixed window is now the
    /// only decision: no reverse delta leaks inside it, regardless of size.
    #[tokio::test(flavor = "current_thread")]
    async fn a_large_reverse_burst_inside_the_window_is_never_forwarded() {
        let fake = FakeConnection::new();
        let sent = fake.sent.clone();
        let mut task = capture_task_with_connection(
            vec![(7, Position::Left, CaptureType::Default)],
            Box::new(fake),
        );
        let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
            .await
            .expect("dummy capture");
        task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
            .await
            .expect("begin");
        task.reverse_guard_until = Some(Instant::now() + Duration::from_secs(60));

        // Observed serial 2 shape: -17.96 residual + -16.94 warp (34.9 px),
        // expanded into per-event bursts around the old budget boundary.
        send_pointer(&mut task, &mut capture, 20.0, 0.0).await;
        send_pointer(&mut task, &mut capture, 16.0, 0.0).await;
        send_pointer(&mut task, &mut capture, 20.0, 0.0).await;
        assert!(
            sent_motions(&sent).is_empty(),
            "no part of a large reverse burst may leak inside the window: {:?}",
            sent.borrow()
        );
        assert_eq!(
            task.residual_reverse_px,
            Some(0.0),
            "a burst past the nominal bound saturates the trace distance"
        );
        assert!(
            task.reverse_guard_until.is_some(),
            "the guard is bounded by the window, not by accumulated distance"
        );

        // Delayed, not lost: once the bounded filter expires the reverse motion
        // passes (the pointer is still near the edge, so only the time bound
        // can end suppression; a held reverse gesture is never blocked longer).
        expire_handoff_filter(&mut task);
        send_pointer(&mut task, &mut capture, 20.0, 0.0).await;
        assert_eq!(
            sent_motions(&sent),
            vec![(20.0, 0.0)],
            "reverse motion after the bounded filter must be forwarded unchanged"
        );
        assert!(task.residual_reverse_px.is_none());
        assert!(task.reverse_guard_until.is_none());
        assert!(task.handoff_inward_px.is_none());
        capture.terminate().await.expect("terminate");
    }

    /// A genuine reverse return after the bounded guard must be forwarded
    /// untouched: an intentional hand-back must never need extra force.
    #[tokio::test(flavor = "current_thread")]
    async fn a_reverse_return_after_the_guard_window_is_forwarded() {
        let fake = FakeConnection::new();
        let sent = fake.sent.clone();
        let mut task = capture_task_with_connection(
            vec![(7, Position::Left, CaptureType::Default)],
            Box::new(fake),
        );
        let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
            .await
            .expect("dummy capture");
        task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
            .await
            .expect("begin");
        task.reverse_guard_until = Some(Instant::now() + Duration::from_secs(60));
        task.handoff_leading_until = Some(Instant::now() + Duration::from_secs(60));

        send_pointer(&mut task, &mut capture, -3.0, 0.0).await;
        // A residual backswing right after the crossing is still guarded.
        send_pointer(&mut task, &mut capture, 5.0, 0.0).await;
        assert_eq!(sent_motions(&sent), vec![(-3.0, 0.0)]);

        // Simulate the bounded hand-off filter elapsing instead of sleeping.
        expire_handoff_filter(&mut task);
        send_pointer(&mut task, &mut capture, 5.0, 0.0).await;
        assert_eq!(
            sent_motions(&sent),
            vec![(-3.0, 0.0), (5.0, 0.0)],
            "a reverse delta after the bounded guard must be forwarded"
        );
        assert!(task.residual_reverse_px.is_none());
        capture.terminate().await.expect("terminate");
    }

    // -----------------------------------------------------------------
    // Old-receiver compatibility (Windows still on release 0.2.18)
    // -----------------------------------------------------------------

    /// Effective reverse margin of the old Windows receiver: it parks the
    /// cursor on the entry edge (1 px inside) and hands control back once the
    /// tracked pointer is pushed 2 px past it, i.e. ~3 px of outward motion.
    /// The model below anchors at the edge itself, which is the strict version
    /// of that behaviour.
    const OLD_RECEIVER_RETURN_MARGIN_PX: f64 = 3.0;

    /// Behavioural model of the release 0.2.18 receiver's return detector: it
    /// tracks the raw position on the hand-off axis and requests the hand-back
    /// once that position is more than the return margin past the entry edge.
    #[derive(Default)]
    struct OldReceiverModel {
        /// Distance out of the entry edge; negative means inside the screen.
        outward_px: f64,
        /// 1 once the receiver has asked for control back (it stops tracking).
        returns: u32,
    }

    impl OldReceiverModel {
        fn apply(&mut self, pos: Position, dx: f64, dy: f64) {
            self.outward_px += handoff_outward_px(pos, dx, dy);
            if self.returns == 0 && self.outward_px > OLD_RECEIVER_RETURN_MARGIN_PX {
                self.returns = 1;
            }
        }
    }

    /// Compatibility contract with the old Windows receiver: the raw delta
    /// sequences from the hand-off trace (activation warp/backswing, forward
    /// deltas, post-forward backswing residual) must not make the old receiver
    /// hand control back. This includes the serial 2 burst (34.9 px reverse)
    /// and the serial 3 single 63 px warp that used to leak whole past the
    /// 32 px accumulation bound. The first forward delta must still move the
    /// old receiver cursor inward immediately, and a deliberate return after
    /// the window must still be delivered.
    ///
    /// Deltas are source-side (`Position::Left`): reverse is positive `dx`,
    /// forward negative, mirroring the trace magnitudes.
    #[tokio::test(flavor = "current_thread")]
    async fn the_reverse_guard_keeps_an_old_receiver_from_handing_control_back() {
        let scenes: [(&str, &[f64]); 3] = [
            (
                "serial1 post-forward backswing",
                &[5.0, 7.0, -2.0, 4.0, 5.0],
            ),
            ("serial2 full backswing", &[17.961, -12.390, 16.938]),
            ("serial3 single 63 px warp", &[63.272, -3.025]),
        ];

        for (label, raw) in scenes {
            let fake = FakeConnection::new();
            let sent = fake.sent.clone();
            let mut task = capture_task_with_connection(
                vec![(7, Position::Left, CaptureType::Default)],
                Box::new(fake),
            );
            let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
                .await
                .expect("dummy capture");
            task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
                .await
                .expect("begin");
            task.reverse_guard_until = Some(Instant::now() + Duration::from_secs(60));
            task.handoff_leading_until = Some(Instant::now() + Duration::from_secs(60));

            let mut receiver = OldReceiverModel::default();
            let mut replayed = 0usize;
            let mut expected_inward = 0.0;
            for &dx in raw {
                send_pointer(&mut task, &mut capture, dx, 0.0).await;
                expected_inward += dx.min(0.0);
                let forwarded = sent_motions(&sent);
                for (fdx, fdy) in &forwarded[replayed..] {
                    receiver.apply(Position::Left, *fdx, *fdy);
                }
                replayed = forwarded.len();
            }

            assert!(
                sent_motions(&sent).iter().all(|(dx, _)| *dx <= 0.0),
                "{label}: no reverse delta may reach the old receiver inside the guard: {:?}",
                sent_motions(&sent)
            );
            assert_eq!(
                receiver.returns, 0,
                "{label}: the old receiver must never hand control back inside the guard"
            );
            assert!(
                (receiver.outward_px - expected_inward).abs() < 1e-9,
                "{label}: only forward motion may move the old receiver cursor, outward={}",
                receiver.outward_px
            );

            // Model sensitivity: the same raw deltas without the guard do hand
            // control back, so the assertions above are meaningful.
            let mut unguarded = OldReceiverModel::default();
            for &dx in raw {
                unguarded.apply(Position::Left, dx, 0.0);
            }
            assert!(
                unguarded.returns > 0,
                "{label}: the old receiver must return without the source-side reverse guard"
            );

            // Delayed, not lost: an intentional hand-back after the bounded
            // filter is forwarded and reaches the old receiver.
            expire_handoff_filter(&mut task);
            send_pointer(&mut task, &mut capture, 20.0, 0.0).await;
            let forwarded = sent_motions(&sent);
            for (fdx, fdy) in &forwarded[replayed..] {
                receiver.apply(Position::Left, *fdx, *fdy);
            }
            assert_eq!(
                receiver.returns, 1,
                "{label}: a reverse push after the bounded filter must reach the old receiver"
            );
            capture.terminate().await.expect("terminate");
        }
    }

    /// A late activation warp that arrives after the fixed telemetry window
    /// while the receiver cursor is still next to the entry edge must still be
    /// suppressed: the source tracks the forwarded displacement, so a pointer
    /// that never moved clearly inward is still in the activation residual
    /// phase (ranked cause #1 of the bidirectional analysis).
    #[tokio::test(flavor = "current_thread")]
    async fn a_late_reverse_residual_is_suppressed_while_the_pointer_is_near_the_edge() {
        // Source-side sign conventions: Position::Left => forward is -dx,
        // Position::Right => forward is +dx (see `handoff_outward_px`).
        for (pos, fwd_sign) in [(Position::Left, -1.0_f64), (Position::Right, 1.0)] {
            let fake = FakeConnection::new();
            let sent = fake.sent.clone();
            let mut task =
                capture_task_with_connection(vec![(7, pos, CaptureType::Default)], Box::new(fake));
            let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
                .await
                .expect("dummy capture");
            task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
                .await
                .expect("begin");

            // A small crossing motion: still inside the entry band.
            send_pointer(&mut task, &mut capture, fwd_sign * 3.0, 0.0).await;
            // The fixed telemetry window expires (simulated), but the pointer
            // has not moved clearly inward; a late ~17 px activation warp
            // arrives.
            task.reverse_guard_until = Some(Instant::now() - Duration::from_millis(1));
            send_pointer(&mut task, &mut capture, -fwd_sign * 17.0, 0.0).await;

            assert_eq!(
                sent_motions(&sent),
                vec![(fwd_sign * 3.0, 0.0)],
                "{pos:?}: a late reverse warp next to the edge must not be forwarded"
            );
            assert_eq!(
                task.handoff_inward_px,
                Some(3.0),
                "{pos:?}: the tracked inward displacement must be integrated from forwarded deltas"
            );
            assert!(
                task.handoff_leading_until.is_some(),
                "{pos:?}: the position-aware filter must still be armed"
            );
            capture.terminate().await.expect("terminate");
        }
    }

    /// No extra force: once the tracked pointer moved clearly inward, reverse
    /// motion is user intent and must be forwarded immediately, even before
    /// the bounded filter expired (position decides, time only bounds).
    #[tokio::test(flavor = "current_thread")]
    async fn a_reverse_gesture_after_clearly_moving_inward_is_forwarded_immediately() {
        for (pos, fwd_sign) in [(Position::Left, -1.0_f64), (Position::Right, 1.0)] {
            let fake = FakeConnection::new();
            let sent = fake.sent.clone();
            let mut task =
                capture_task_with_connection(vec![(7, pos, CaptureType::Default)], Box::new(fake));
            let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
                .await
                .expect("dummy capture");
            task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
                .await
                .expect("begin");

            for _ in 0..5 {
                send_pointer(&mut task, &mut capture, fwd_sign * 12.0, 0.0).await;
            }
            assert_eq!(
                sent_motions(&sent).len(),
                5,
                "{pos:?}: every forward delta must be forwarded immediately"
            );
            assert!(
                task.handoff_inward_px.is_none(),
                "{pos:?}: 60 px inward is clearly inside; the residual filter must disarm"
            );

            let reverse = -fwd_sign * 10.0;
            send_pointer(&mut task, &mut capture, reverse, 0.0).await;
            assert_eq!(
                sent_motions(&sent).last().copied(),
                Some((reverse, 0.0)),
                "{pos:?}: a deliberate reverse after moving inward must be forwarded"
            );
            capture.terminate().await.expect("terminate");
        }
    }

    /// The post-forward backswing (serial 2 scene: -17.961 residual, +12.390
    /// crossing, -16.938 backswing) must stay suppressed: 12.39 px inward is
    /// still inside the entry band, so the backswing would still reach the
    /// receiver parked next to its entry edge.
    #[tokio::test(flavor = "current_thread")]
    async fn the_leading_forward_motion_keeps_the_residual_filter_alive() {
        let fake = FakeConnection::new();
        let sent = fake.sent.clone();
        let mut task = capture_task_with_connection(
            vec![(7, Position::Left, CaptureType::Default)],
            Box::new(fake),
        );
        let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
            .await
            .expect("dummy capture");
        task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
            .await
            .expect("begin");

        send_pointer(&mut task, &mut capture, 17.961, 0.0).await; // residual
        send_pointer(&mut task, &mut capture, -12.390, 0.0).await; // crossing
        send_pointer(&mut task, &mut capture, 16.938, 0.0).await; // backswing

        assert_eq!(
            sent_motions(&sent),
            vec![(-12.390, 0.0)],
            "the crossing motion is forwarded; both reverse residuals are suppressed"
        );
        let inward = task.handoff_inward_px.expect("filter still armed");
        assert!(
            (inward - 12.390).abs() < 1e-9,
            "the forwarded crossing motion must be integrated, got {inward}"
        );
        capture.terminate().await.expect("terminate");
    }

    // -----------------------------------------------------------------
    // Receiver-side incoming-capture take-back gate (the along=None storm)
    // -----------------------------------------------------------------

    /// Next `ICaptureEvent` surfaced by the task, or `None` if none arrives.
    async fn next_capture_event(rx: &mut Receiver<ICaptureEvent>) -> Option<ICaptureEvent> {
        tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .ok()
            .flatten()
    }

    /// Assert that no `ICaptureEvent` is surfaced within a short window.
    async fn no_capture_event(rx: &mut Receiver<ICaptureEvent>) -> bool {
        tokio::time::timeout(Duration::from_millis(20), rx.recv())
            .await
            .is_err()
    }

    /// While a remote peer drives this machine, a `Begin` on the incoming
    /// (`EnterOnly`) capture during the entry dwell is settling jitter: it must
    /// not surface `ICaptureEvent::CaptureBegin` (the service turns that into
    /// `request_handoff(addr, None)` - the `along=None` storm).
    #[tokio::test(flavor = "current_thread")]
    async fn an_incoming_begin_during_the_remote_entry_dwell_is_not_a_take_back() {
        let (mut task, mut events) = capture_task_with_events(
            vec![(7, Position::Right, CaptureType::EnterOnly)],
            Box::new(FakeConnection::new()),
        );
        task.update_remote_peer(Some(lan_mouse_ipc::Position::Right));
        assert!(
            task.remote_peer_since.is_some(),
            "the dwell must be anchored when the peer starts driving"
        );
        let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
            .await
            .expect("dummy capture");
        capture.create(7, Position::Right).await.expect("create");

        task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
            .await
            .expect("begin");

        assert!(
            no_capture_event(&mut events).await,
            "a begin during the remote-entry dwell must not request a take-back"
        );
        assert!(
            !task.enter_only_take_back_sent,
            "a suppressed jitter begin must not consume the session's request"
        );
        capture.terminate().await.expect("terminate");
    }

    /// A deliberate local take-back after the dwell is still surfaced - exactly
    /// once per remote-driving session; the service retries the request.
    #[tokio::test(flavor = "current_thread")]
    async fn a_deliberate_take_back_after_the_dwell_is_reported_once() {
        let (mut task, mut events) = capture_task_with_events(
            vec![(7, Position::Right, CaptureType::EnterOnly)],
            Box::new(FakeConnection::new()),
        );
        task.update_remote_peer(Some(lan_mouse_ipc::Position::Right));
        task.remote_peer_since =
            Some(Instant::now() - REMOTE_ENTRY_DWELL - Duration::from_millis(1));
        let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
            .await
            .expect("dummy capture");
        capture.create(7, Position::Right).await.expect("create");

        task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
            .await
            .expect("begin");
        assert!(
            matches!(
                next_capture_event(&mut events).await,
                Some(ICaptureEvent::CaptureBegin(7))
            ),
            "a deliberate take-back after the dwell must be reported"
        );
        assert!(task.enter_only_take_back_sent);

        // Further begins in the same driving session are injected-pointer
        // jitter: the service already retries the request.
        task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
            .await
            .expect("begin");
        assert!(
            no_capture_event(&mut events).await,
            "one take-back request per remote-driving session is enough"
        );
        capture.terminate().await.expect("terminate");
    }

    /// A new remote-driving session (the peer stopped and re-entered) re-arms
    /// the take-back gate, so a later deliberate local take-back works again.
    #[tokio::test(flavor = "current_thread")]
    async fn the_take_back_gate_rearms_for_a_new_remote_session() {
        let (mut task, mut events) = capture_task_with_events(
            vec![(7, Position::Right, CaptureType::EnterOnly)],
            Box::new(FakeConnection::new()),
        );
        let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
            .await
            .expect("dummy capture");
        capture.create(7, Position::Right).await.expect("create");

        task.update_remote_peer(Some(lan_mouse_ipc::Position::Right));
        task.remote_peer_since =
            Some(Instant::now() - REMOTE_ENTRY_DWELL - Duration::from_millis(1));
        task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
            .await
            .expect("begin");
        assert!(matches!(
            next_capture_event(&mut events).await,
            Some(ICaptureEvent::CaptureBegin(7))
        ));

        task.update_remote_peer(None);
        assert!(!task.enter_only_take_back_sent, "the gate must reset");
        task.update_remote_peer(Some(lan_mouse_ipc::Position::Right));
        task.remote_peer_since =
            Some(Instant::now() - REMOTE_ENTRY_DWELL - Duration::from_millis(1));
        assert!(
            !task.enter_only_take_back_sent,
            "a new session starts unarmed"
        );
        task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
            .await
            .expect("begin");
        assert!(
            matches!(
                next_capture_event(&mut events).await,
                Some(ICaptureEvent::CaptureBegin(7))
            ),
            "the new session's take-back must be reported"
        );
        capture.terminate().await.expect("terminate");
    }

    #[test]
    fn the_reverse_guard_stays_short_and_small() {
        // Runtime checks (not const-eval) keep clippy quiet.
        assert_eq!(HANDOFF_REVERSE_GUARD.as_millis(), 200);
        let budget = HANDOFF_RESIDUAL_MAX_PX;
        assert!(
            budget.is_finite() && budget > 0.0 && budget <= 32.0,
            "the reported reverse residual must stay small: {budget} px"
        );
        let band = HANDOFF_ENTRY_BAND_PX;
        assert!(
            band.is_finite() && (8.0..=24.0).contains(&band),
            "the position-aware entry band must stay a few logical px: {band} px"
        );
        // Wider than the largest observed leading forward delta (12.39 px) so
        // the post-forward backswing is still suppressed.
        assert!(
            band > 12.39,
            "the band must cover the leading forward delta"
        );
        let position_guard = HANDOFF_POSITION_GUARD;
        assert!(
            position_guard > HANDOFF_REVERSE_GUARD && position_guard.as_millis() <= 300,
            "the bounded position guard must stay short: {position_guard:?}"
        );
    }

    /// Send one pointer motion for handle 7; the delta sign conventions are the
    /// source-side ones (`Position::Left` = peer on the left, forward = -dx).
    async fn send_pointer(task: &mut CaptureTask, capture: &mut InputCapture, dx: f64, dy: f64) {
        task.handle_capture_event(
            capture,
            (
                7,
                CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { time: 0, dx, dy })),
            ),
        )
        .await
        .expect("pointer motion");
    }

    /// Pointer motions actually forwarded to the peer, in send order.
    fn sent_motions(sent: &Rc<RefCell<Vec<ProtoEvent>>>) -> Vec<(f64, f64)> {
        sent.borrow()
            .iter()
            .filter_map(|event| match event {
                ProtoEvent::Input(Event::Pointer(PointerEvent::Motion { dx, dy, .. })) => {
                    Some((*dx, *dy))
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn legacy_peer_gets_plain_enter() {
        let fake = FakeConnection {
            supports_edge_coordinate: false,
            ..FakeConnection::new()
        };
        let sent = fake.sent.clone();
        let mut task = capture_task_with_connection(
            vec![(7, Position::Left, CaptureType::Default)],
            Box::new(fake),
        );
        let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
            .await
            .expect("dummy capture");
        task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
            .await
            .expect("begin");
        assert!(
            matches!(
                sent.borrow().first(),
                Some(ProtoEvent::Enter { serial: 1, .. })
            ),
            "legacy peer must get Enter: {:?}",
            sent.borrow()
        );
        capture.terminate().await.expect("terminate");
    }

    /// Regression: clicking Connect while the peer is unreachable installs
    /// capture barriers for the active client. Reaching the edge used to arm
    /// the grab and send `Enter` into a dead link, leaving the local pointer
    /// locked at the edge until the retry timeout (and fighting the compositor
    /// on release). A begin without an established transport must give the
    /// backend back immediately, before taking the grab.
    #[tokio::test(flavor = "current_thread")]
    async fn a_begin_without_a_connection_does_not_leave_the_pointer_grabbed() {
        let fake = FakeConnection {
            connected: false,
            ..FakeConnection::new()
        };
        let sent = fake.sent.clone();
        let mut task = capture_task_with_connection(
            vec![(7, Position::Top, CaptureType::Default)],
            Box::new(fake),
        );
        let mut capture = InputCapture::new(Some(input_capture::Backend::Dummy))
            .await
            .expect("dummy capture");

        task.handle_capture_event(&mut capture, (7, CaptureEvent::BeginAt(0.5)))
            .await
            .expect("a disconnected begin must release, not fail");

        assert!(
            task.active_client.is_none(),
            "the capture must not stay armed for an unreachable peer"
        );
        assert!(
            matches!(task.state, State::WaitingForAck),
            "state must be reset, not left as Sending/Waiting for a dead peer"
        );
        assert!(
            sent.borrow().is_empty(),
            "no Enter may be sent into a link that is not connected"
        );
        assert_eq!(task.enter.awaiting_ack(), None);

        capture.terminate().await.expect("terminate");
    }
}
