//! Independent verification harness for the old-Windows-receiver hand-off fix.
//!
//! The reported regression: after the Ubuntu touchpad crosses into the Windows
//! host, the **old Windows 0.2.18 receiver** (`RETURN_MARGIN_PX = 3.0`, anchor
//! exactly at the `EnterAt` edge) keeps handing control back (`LeaveAt`) while
//! the user is still moving into the Windows screen. The source-side residual
//! filter only suppressed reverse deltas *before* the first forward delta, so
//! the touchpad backswing that arrives after the crossing motion leaked to the
//! receiver and triggered the return.
//!
//! This file verifies the fix from two directions:
//!
//! 1. **Production replay** (`mod capture`, compiled from `src/capture.rs`
//!    read-only with tiny stubs for the unrelated connection/client modules):
//!    the real `CaptureTask` state machine is fed trace-shaped delta
//!    sequences through `handle_capture_event`. It is not a reimplementation:
//!    the exact production file that ships is compiled into this test binary.
//!    The contract checked is the final one: inside the fixed 200 ms
//!    post-`Begin` window **all** reverse deltas are suppressed, including
//!    activation bursts larger than the old 32 px accumulation budget; after
//!    the window reverse motion is forwarded (a deliberate hand-back is
//!    delayed by at most the window, never lost).
//! 2. **Independent old-receiver model**: a faithful, separately written model
//!    of the Windows 0.2.18 absolute tracker (v0.2.18 tag `5233b05`) consumes
//!    the deltas the production task actually forwarded and reports whether it
//!    would fire a return. The same model fed the raw (unguarded) trace is used
//!    as a sensitivity check: it must fire, proving the test can detect the
//!    bug.
//!
//! The cursor-hide/cursor-park shutdown contract is re-checked here from the
//! real `src/cursor_hide.rs` (same compiled-source technique); the end-to-end
//! daemon lifecycle is covered by `scripts/qa-handoff-shutdown.sh`.
//!
//! Layer: L2 deterministic contract tests. No sockets, no sleeps, no display.

#![allow(dead_code)]

// ---------------------------------------------------------------------------
// Minimal stubs for the modules `src/capture.rs` pulls in.
//
// Only the items the capture module (including its own unit tests) references
// are provided. Nothing here is under test: the connection and client manager
// are replaced by the in-memory `ReplayConnection` inside `mod capture`.
// ---------------------------------------------------------------------------

mod client {
    use std::cell::RefCell;

    /// Records the peer capabilities the stub connection queries.
    #[derive(Clone, Default)]
    pub struct ClientManager {
        caps: RefCell<Vec<Option<u32>>>,
    }

    impl ClientManager {
        pub(crate) fn add_with_config(
            &self,
            _config: crate::config::ConfigClient,
        ) -> lan_mouse_ipc::ClientHandle {
            let mut caps = self.caps.borrow_mut();
            caps.push(None);
            (caps.len() - 1) as lan_mouse_ipc::ClientHandle
        }

        pub(crate) fn set_peer_hello(
            &self,
            handle: lan_mouse_ipc::ClientHandle,
            _commit: Option<[u8; 8]>,
            _proto_version: u8,
            capabilities: u32,
            _app_version: Option<lan_mouse_proto::AppVersion>,
        ) {
            let mut caps = self.caps.borrow_mut();
            if let Some(slot) = caps.get_mut(handle as usize) {
                *slot = Some(capabilities);
            }
        }

        pub(crate) fn peer_capabilities(&self, handle: lan_mouse_ipc::ClientHandle) -> Option<u32> {
            self.caps.borrow().get(handle as usize).copied().flatten()
        }
    }
}

mod config {
    use std::{collections::HashSet, net::IpAddr};

    /// Copy of the production config entry shape used by one capture unit test.
    pub struct ConfigClient {
        pub ips: HashSet<IpAddr>,
        pub hostname: Option<String>,
        pub port: u16,
        pub pos: lan_mouse_ipc::Position,
        pub active: bool,
        pub enter_hook: Option<String>,
        pub leave_hook: Option<String>,
        pub sensitivity: u16,
        pub fingerprint: Option<String>,
    }
}

mod connect {
    use std::time::Duration;

    use input_capture::CaptureHandle;
    use lan_mouse_proto::ProtoEvent;

    #[derive(Debug)]
    pub(crate) enum LanMouseConnectionError {
        NotConnected,
    }

    impl std::fmt::Display for LanMouseConnectionError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::NotConnected => f.write_str("not connected"),
            }
        }
    }

    impl std::error::Error for LanMouseConnectionError {}

    /// Stub of the real DTLS connection. The replay drives the capture task
    /// through its `CaptureConnection` seam instead, so these methods are only
    /// needed to make the production source compile.
    pub(crate) struct LanMouseConnection {
        manager: crate::client::ClientManager,
    }

    impl LanMouseConnection {
        pub(crate) fn new(
            _cert: webrtc_dtls::crypto::Certificate,
            manager: crate::client::ClientManager,
            _connect_grace_ms: u64,
        ) -> Self {
            Self { manager }
        }

        pub(crate) fn peer_supports_enter_ack(&self, handle: CaptureHandle) -> bool {
            self.manager
                .peer_capabilities(handle)
                .is_some_and(|caps| caps & lan_mouse_proto::capabilities::ENTER_ACK != 0)
        }

        pub(crate) fn peer_supports_serialized_leave(&self, handle: CaptureHandle) -> bool {
            self.manager
                .peer_capabilities(handle)
                .is_some_and(|caps| caps & lan_mouse_proto::capabilities::SERIALIZED_LEAVE != 0)
        }

        pub(crate) fn peer_supports_edge_coordinate(&self, handle: CaptureHandle) -> bool {
            self.manager
                .peer_capabilities(handle)
                .is_some_and(|caps| caps & lan_mouse_proto::capabilities::EDGE_COORDINATE != 0)
        }

        pub(crate) fn peer_rtt_ms(&self, _handle: CaptureHandle) -> Option<u32> {
            None
        }

        pub(crate) fn is_connected(&self, _handle: CaptureHandle) -> bool {
            true
        }

        pub(crate) fn ensure_connected(&self, _handle: CaptureHandle) {}

        pub(crate) async fn send_with_grace(
            &self,
            _event: ProtoEvent,
            _handle: CaptureHandle,
            _grace: Duration,
        ) -> Result<(), LanMouseConnectionError> {
            Ok(())
        }

        pub(crate) async fn send_now(
            &self,
            _event: ProtoEvent,
            _handle: CaptureHandle,
        ) -> Result<(), LanMouseConnectionError> {
            Ok(())
        }

        pub(crate) async fn recv(&mut self) -> (CaptureHandle, ProtoEvent) {
            std::future::pending().await
        }

        pub(crate) async fn disconnect(&self, _handle: CaptureHandle) {}

        pub(crate) async fn drop_transport(&self, _handle: CaptureHandle) {}
    }
}

#[path = "../src/coalesce.rs"]
mod coalesce;
#[path = "../src/crypto.rs"]
mod crypto;
#[path = "../src/cursor_hide.rs"]
mod cursor_hide;
#[path = "../src/handoff_trace.rs"]
mod handoff_trace;

// ---------------------------------------------------------------------------
// Production capture task, compiled read-only from the shipping source file.
//
// The module is wrapped in `mod capture` so the exact `src/capture.rs` that
// ships is compiled into this test binary, and the driver appended after the
// `include!` can reach the private `CaptureTask` internals (the same technique
// the previous verifier used for `cursor_hide`/`coalesce`).
// ---------------------------------------------------------------------------

mod capture {
    include!("../src/capture.rs");

    // ---- verifier replay driver (test-only code, not production) -----------

    /// Peer capabilities of the old Windows receiver under test: the v0.2.18
    /// release (`5233b05`) advertises `ENTER_ACK`, `SERIALIZED_LEAVE` and
    /// `EDGE_COORDINATE`, so it is an "old but modern-protocol" peer: the
    /// source arms the entry guard, sends `EnterAt` and waits for the Ack.
    struct ReplayConnection {
        sent: Rc<RefCell<Vec<ProtoEvent>>>,
    }

    #[async_trait::async_trait(?Send)]
    impl CaptureConnection for ReplayConnection {
        fn peer_supports_enter_ack(&self, _handle: CaptureHandle) -> bool {
            true
        }

        fn peer_supports_serialized_leave(&self, _handle: CaptureHandle) -> bool {
            true
        }

        fn peer_supports_edge_coordinate(&self, _handle: CaptureHandle) -> bool {
            true
        }

        fn peer_rtt_ms(&self, _handle: CaptureHandle) -> Option<u32> {
            Some(2)
        }

        fn is_connected(&self, _handle: CaptureHandle) -> bool {
            true
        }

        fn ensure_connected(&self, _handle: CaptureHandle) {}

        async fn send_with_grace(
            &self,
            event: ProtoEvent,
            _handle: CaptureHandle,
            _grace: Duration,
        ) -> Result<(), LanMouseConnectionError> {
            self.sent.borrow_mut().push(event);
            Ok(())
        }

        async fn send_now(
            &self,
            event: ProtoEvent,
            _handle: CaptureHandle,
        ) -> Result<(), LanMouseConnectionError> {
            self.sent.borrow_mut().push(event);
            Ok(())
        }

        async fn recv(&mut self) -> (CaptureHandle, ProtoEvent) {
            std::future::pending().await
        }

        async fn disconnect(&self, _handle: CaptureHandle) {}

        async fn drop_transport(&self, _handle: CaptureHandle) {}
    }

    /// Build a `CaptureTask` with coalescing disabled (batch 0), so the replay
    /// observes the hand-off filter output event by event.
    fn replay_task(
        captures: Vec<(CaptureHandle, Position, CaptureType)>,
    ) -> (CaptureTask, Rc<RefCell<Vec<ProtoEvent>>>) {
        let (event_tx, _event_rx) = channel();
        let (_request_tx, request_rx) = channel();
        let sent: Rc<RefCell<Vec<ProtoEvent>>> = Rc::new(RefCell::new(Vec::new()));
        let conn = ReplayConnection { sent: sent.clone() };
        let task = CaptureTask {
            active_client: None,
            backend: Some(input_capture::Backend::Dummy),
            cancellation_token: CancellationToken::new(),
            captures,
            coalescer: MotionCoalescer::new(0),
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
        (task, sent)
    }

    async fn replay_begin_at(
        task: &mut CaptureTask,
        capture: &mut InputCapture,
        handle: CaptureHandle,
        along: f32,
    ) {
        task.handle_capture_event(capture, (handle, CaptureEvent::BeginAt(along)))
            .await
            .expect("begin event");
    }

    /// Simulate the old receiver's prompt `Ack`: the real one acknowledges in
    /// the same millisecond it arms (see `docs/testing/handoff-trace.md`).
    fn replay_ack(task: &mut CaptureTask) {
        assert_eq!(
            task.state,
            State::WaitingForAck,
            "the replay models a modern peer: the capture waits for the Ack"
        );
        task.state = State::Sending;
        task.enter.reset();
    }

    async fn replay_motion(
        task: &mut CaptureTask,
        capture: &mut InputCapture,
        handle: CaptureHandle,
        dx: f64,
        dy: f64,
    ) {
        task.handle_capture_event(
            capture,
            (
                handle,
                CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { time: 0, dx, dy })),
            ),
        )
        .await
        .expect("motion event");
    }

    /// Simulate the whole hand-off guard elapsing without sleeping: the old
    /// fixed 200 ms window plus the position-aware bound added by task-10.
    /// The position-aware filter only disarms early once the pointer moved
    /// clearly inward, so a "return after the window" needs both deadlines
    /// expired (the 250 ms bounded escape).
    fn replay_expire_reverse_guard(task: &mut CaptureTask) {
        assert!(
            task.handoff_inward_px.is_some(),
            "precondition: the position-aware guard must be armed after Begin"
        );
        let past = Some(Instant::now() - Duration::from_millis(1));
        task.reverse_guard_until = past;
        task.handoff_leading_until = past;
    }

    /// The pointer motions the source actually forwarded, in send order.
    fn forwarded_motions(sent: &[ProtoEvent]) -> Vec<(f64, f64)> {
        sent.iter()
            .filter_map(|event| match event {
                ProtoEvent::Input(Event::Pointer(PointerEvent::Motion { dx, dy, .. })) => {
                    Some((*dx, *dy))
                }
                _ => None,
            })
            .collect()
    }

    // ---- production replay tests (module scope: they need CaptureTask) ----

    /// The crossing motion must be forwarded in the very call that delivers it,
    /// with its exact delta: no timed settle window, no rescaling, no "extra
    /// force" and no wait for a batch/ack timer.
    #[tokio::test(flavor = "current_thread")]
    async fn the_first_forward_delta_is_forwarded_immediately_with_its_exact_value() {
        let (mut task, sent) = replay_task(vec![(
            crate::HANDLE,
            crate::SOURCE_POS,
            CaptureType::Default,
        )]);
        let mut input_capture = crate::dummy_capture().await;

        replay_begin_at(&mut task, &mut input_capture, crate::HANDLE, 0.5).await;
        assert!(
            sent.borrow()
                .iter()
                .any(|event| matches!(event, lan_mouse_proto::ProtoEvent::EnterAt { .. })),
            "the hand-off must start with EnterAt for the modern old receiver"
        );

        // Deliberately do NOT send the Ack first: the first crossing motion must
        // not be gated on the handshake round trip.
        replay_motion(&mut task, &mut input_capture, crate::HANDLE, 2.0, 0.0).await;

        let forwarded = forwarded_motions(&sent.borrow());
        assert_eq!(
            forwarded,
            vec![(2.0, 0.0)],
            "the first forward motion must be forwarded at once and unchanged"
        );

        let mut receiver = crate::old_receiver::OldWindowsReceiver::new(
            crate::old_receiver::OldWindowsReceiver::edge_for_capture_pos(crate::SOURCE_POS),
        );
        receiver.feed(&forwarded);
        assert!(
            receiver.returns.is_empty(),
            "the first forward motion must move the old receiver inward, not hand back"
        );

        input_capture.terminate().await.expect("terminate");
    }

    /// The reported regression: the touchpad backswing arrives *after* the first
    /// forward delta. The fixed source keeps suppressing reverse-only motion for
    /// the short post-`Begin` guard, so the old receiver never sees the outward
    /// push and never issues `LeaveAt`.
    #[tokio::test(flavor = "current_thread")]
    async fn the_post_forward_backswing_is_suppressed_and_the_old_receiver_never_returns() {
        let (mut task, sent) = replay_task(vec![(
            crate::HANDLE,
            crate::SOURCE_POS,
            CaptureType::Default,
        )]);
        let mut input_capture = crate::dummy_capture().await;

        replay_begin_at(&mut task, &mut input_capture, crate::HANDLE, 0.5).await;
        replay_ack(&mut task);
        for (dx, dy) in crate::RAW_TRACE {
            replay_motion(&mut task, &mut input_capture, crate::HANDLE, dx, dy).await;
        }

        let forwarded = forwarded_motions(&sent.borrow());
        assert_eq!(
            forwarded,
            vec![(3.0, 0.0), (4.0, 0.0)],
            "only the forward deltas may reach the old receiver (got {forwarded:?})"
        );

        let mut receiver = crate::old_receiver::OldWindowsReceiver::new(
            crate::old_receiver::OldWindowsReceiver::edge_for_capture_pos(crate::SOURCE_POS),
        );
        let returns = receiver.feed(&forwarded);
        assert_eq!(
            returns, 0,
            "the old receiver must not hand control back during the crossing: {:?}",
            receiver.returns
        );
        // Both forward deltas moved the cursor inward: +3 and +4.
        assert!(
            (receiver.x - 7.0).abs() < 1e-9,
            "the cursor must move into the Windows screen immediately, x={}",
            receiver.x
        );

        input_capture.terminate().await.expect("terminate");
    }

    /// The guard is reverse-only and bounded: a real reverse gesture after the
    /// window must still be forwarded immediately, so returning to the Ubuntu
    /// machine keeps working.
    #[tokio::test(flavor = "current_thread")]
    async fn a_genuine_return_after_the_guard_window_is_forwarded() {
        let (mut task, sent) = replay_task(vec![(
            crate::HANDLE,
            crate::SOURCE_POS,
            CaptureType::Default,
        )]);
        let mut input_capture = crate::dummy_capture().await;

        replay_begin_at(&mut task, &mut input_capture, crate::HANDLE, 0.5).await;
        replay_ack(&mut task);
        replay_motion(&mut task, &mut input_capture, crate::HANDLE, 3.0, 0.0).await;
        replay_expire_reverse_guard(&mut task);
        replay_motion(&mut task, &mut input_capture, crate::HANDLE, -8.0, 0.0).await;

        let forwarded = forwarded_motions(&sent.borrow());
        assert_eq!(
            forwarded,
            vec![(3.0, 0.0), (-8.0, 0.0)],
            "a reverse gesture after the guard window is user intent and must pass"
        );

        let mut receiver = crate::old_receiver::OldWindowsReceiver::new(
            crate::old_receiver::OldWindowsReceiver::edge_for_capture_pos(crate::SOURCE_POS),
        );
        receiver.feed(&forwarded);
        assert_eq!(
            receiver.returns.len(),
            1,
            "the genuine return push must reach the old receiver and hand back"
        );

        input_capture.terminate().await.expect("terminate");
    }

    /// Load-bearing scenes from the local hand-off trace (task-5's trace
    /// basis, replayed here independently): inside the fixed 200 ms window the
    /// guard must suppress **all** reverse motion, not only the part that fits
    /// a small accumulation budget. A large activation burst (serial 2:
    /// -17.961 px dropped + -16.938 px warp; or a single -63.272 px serial 3
    /// first motion) used to be forwarded whole as a "deliberate hand-back" and
    /// still triggered the old receiver's LeaveAt.
    #[tokio::test(flavor = "current_thread")]
    async fn large_reverse_bursts_inside_the_guard_never_reach_the_old_receiver() {
        // (label, raw capture deltas) — serial 2 / serial 3 records from the
        // local trace, in capture order.
        let scenes: [(&str, &[(f64, f64)]); 3] = [
            (
                "serial2 full backswing (-17.961, +12.390, -16.938)",
                &[(-17.961, 0.0), (12.390, 0.0), (-16.938, 0.0)],
            ),
            (
                "serial2 double residual (-17.961, -16.938, +3.025)",
                &[(-17.961, 0.0), (-16.938, 0.0), (3.025, 0.0)],
            ),
            (
                "serial3 single -63.272 px first motion",
                &[(-63.272, 0.0), (3.025, 0.0)],
            ),
        ];

        for (label, raw) in scenes {
            let (mut task, sent) = replay_task(vec![(
                crate::HANDLE,
                crate::SOURCE_POS,
                CaptureType::Default,
            )]);
            let mut input_capture = crate::dummy_capture().await;
            replay_begin_at(&mut task, &mut input_capture, crate::HANDLE, 0.5).await;
            replay_ack(&mut task);
            let mut reversed = 0.0;
            for &(dx, dy) in raw {
                reversed += (-dx).max(0.0);
                replay_motion(&mut task, &mut input_capture, crate::HANDLE, dx, dy).await;
            }
            assert!(
                reversed > 32.0,
                "{label}: scene must exceed the old 32 px budget"
            );

            let forwarded = forwarded_motions(&sent.borrow());
            assert!(
                forwarded.iter().all(|(dx, _)| *dx > 0.0),
                "{label}: no reverse delta may be forwarded inside the guard, got {forwarded:?}"
            );
            assert!(
                !forwarded.is_empty(),
                "{label}: the forward crossing motion must still be forwarded"
            );

            let mut receiver = crate::old_receiver::OldWindowsReceiver::new(
                crate::old_receiver::OldWindowsReceiver::edge_for_capture_pos(crate::SOURCE_POS),
            );
            assert_eq!(
                receiver.feed(&forwarded),
                0,
                "{label}: the old receiver must never hand control back"
            );
            input_capture.terminate().await.expect("terminate");
        }
    }

    /// The same post-forward backswing is suppressed for every entry edge, which
    /// covers the reverse direction (Windows source -> Ubuntu receiver) and the
    /// top/bottom edges as well.
    #[tokio::test(flavor = "current_thread")]
    async fn every_entry_edge_suppresses_the_post_forward_backswing() {
        for pos in [
            input_capture::Position::Left,
            input_capture::Position::Right,
            input_capture::Position::Top,
            input_capture::Position::Bottom,
        ] {
            let receiver_edge = crate::old_receiver::OldWindowsReceiver::edge_for_capture_pos(pos);
            let (fwd_dx, fwd_dy) = crate::old_receiver::capture_delta(receiver_edge, 0.0, 3.0);
            let (small_rev_dx, small_rev_dy) =
                crate::old_receiver::capture_delta(receiver_edge, 5.0, 0.0);
            let (backswing_dx, backswing_dy) =
                crate::old_receiver::capture_delta(receiver_edge, 17.0, 0.0);
            let (fwd2_dx, fwd2_dy) = crate::old_receiver::capture_delta(receiver_edge, 0.0, 4.0);

            let (mut task, sent) = replay_task(vec![(crate::HANDLE, pos, CaptureType::Default)]);
            let mut input_capture = crate::dummy_capture().await;
            replay_begin_at(&mut task, &mut input_capture, crate::HANDLE, 0.5).await;
            replay_ack(&mut task);
            for (dx, dy) in [
                (small_rev_dx, small_rev_dy),
                (fwd_dx, fwd_dy),
                (backswing_dx, backswing_dy),
                (fwd2_dx, fwd2_dy),
            ] {
                replay_motion(&mut task, &mut input_capture, crate::HANDLE, dx, dy).await;
            }

            let forwarded = forwarded_motions(&sent.borrow());
            assert_eq!(
                forwarded,
                vec![
                    crate::old_receiver::capture_delta(receiver_edge, 0.0, 3.0),
                    crate::old_receiver::capture_delta(receiver_edge, 0.0, 4.0)
                ],
                "{pos:?}: only forward deltas may be forwarded"
            );

            let mut receiver = crate::old_receiver::OldWindowsReceiver::new(receiver_edge);
            assert_eq!(
                receiver.feed(&forwarded),
                0,
                "{pos:?}: the old receiver must not return during the crossing"
            );
            input_capture.terminate().await.expect("terminate");
        }
    }
}

// ---------------------------------------------------------------------------
// Independent model of the Windows 0.2.18 absolute receiver tracker.
//
// Written from the v0.2.18 tag (`5233b05`, `input-emulation/src/libei.rs`):
//
//  * the pointer is warped to the exact `EnterAt` coordinate, which for the
//    entry edge is the edge pixel itself (`entry_point_at`: Left -> r.0,
//    Right -> r.2 - 1, Top -> r.1, Bottom -> r.3 - 1);
//  * `RETURN_MARGIN_PX = 3.0`; the return fires only when the *raw* tracked
//    position is pushed past the entry edge by more than the margin
//    (Left: raw_x < x0 - 3; Right: raw_x > x1 + 3; Top/Bottom analogous);
//  * the position sent to the compositor is clamped into the region, and the
//    tracker mirrors that clamp, so repeated small pushes do not accumulate;
//  * both the "near the entry edge" band and the outward push are required;
//  * after firing, the tracker re-arms only once the pointer is 20 px inside.
//
// The model is deliberately independent from the production source: the point
// of the verification is that the deltas produced by the real source task do
// not trip an old receiver.
// ---------------------------------------------------------------------------

mod old_receiver {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Edge {
        Left,
        Right,
        Top,
        Bottom,
    }

    /// The v0.2.18 return margin, in logical pixels.
    pub const RETURN_MARGIN_PX: f64 = 3.0;
    /// The v0.2.18 re-arm inset after a return.
    pub const REARM_INSET_PX: f64 = 20.0;

    pub const REGION: (f64, f64, f64, f64) = (0.0, 0.0, 1920.0, 1080.0);

    #[derive(Clone, Debug)]
    pub struct OldWindowsReceiver {
        pub edge: Edge,
        pub x: f64,
        pub y: f64,
        pub fired: bool,
        /// Every return this receiver asked for, in order.
        pub returns: Vec<(f64, f64)>,
    }

    impl OldWindowsReceiver {
        pub fn new(edge: Edge) -> Self {
            let (x0, y0, x1, y1) = REGION;
            let (x, y) = match edge {
                // Exact `EnterAt` anchor of the v0.2.18 edge coordinate path.
                Edge::Left => (x0, (y0 + y1) / 2.0),
                Edge::Right => (x1 - 1.0, (y0 + y1) / 2.0),
                Edge::Top => ((x0 + x1) / 2.0, y0),
                Edge::Bottom => ((x0 + x1) / 2.0, y1 - 1.0),
            };
            Self {
                edge,
                x,
                y,
                fired: false,
                returns: Vec::new(),
            }
        }

        /// The receiver edge the pointer is entering through when it crosses
        /// from the local capture at `pos` (`pos` is the side the pointer
        /// leaves the source through).
        pub fn edge_for_capture_pos(pos: input_capture::Position) -> Edge {
            match pos {
                input_capture::Position::Left => Edge::Right,
                input_capture::Position::Right => Edge::Left,
                input_capture::Position::Top => Edge::Bottom,
                input_capture::Position::Bottom => Edge::Top,
            }
        }

        /// Apply one injected delta. Returns `true` when this delta triggered
        /// the return hand-off (a `LeaveAt` to the source).
        pub fn apply(&mut self, dx: f64, dy: f64) -> bool {
            let (x0, y0, x1, y1) = REGION;
            let raw_x = self.x + dx;
            let raw_y = self.y + dy;

            if self.fired {
                self.x = raw_x.clamp(x0, x1 - 1.0);
                self.y = raw_y.clamp(y0, y1 - 1.0);
                if self.x > x0 + REARM_INSET_PX
                    && self.x < x1 - REARM_INSET_PX
                    && self.y > y0 + REARM_INSET_PX
                    && self.y < y1 - REARM_INSET_PX
                {
                    self.fired = false;
                }
                return false;
            }

            // v0.2.18 clamps into the region before deciding, so a small
            // repeated outward push starts from the edge again each time.
            let x = raw_x.clamp(x0, x1 - 1.0);
            let y = raw_y.clamp(y0, y1 - 1.0);
            let near_edge = match self.edge {
                Edge::Left => x <= x0 + 1.0,
                Edge::Right => x >= x1 - 2.0,
                Edge::Top => y <= y0 + 1.0,
                Edge::Bottom => y >= y1 - 2.0,
            };
            let past_edge = match self.edge {
                Edge::Left => raw_x < x0 - RETURN_MARGIN_PX,
                Edge::Right => raw_x > x1 + RETURN_MARGIN_PX,
                Edge::Top => raw_y < y0 - RETURN_MARGIN_PX,
                Edge::Bottom => raw_y > y1 + RETURN_MARGIN_PX,
            };
            self.x = x;
            self.y = y;
            if near_edge && past_edge {
                self.fired = true;
                match self.edge {
                    Edge::Left => self.x = x0,
                    Edge::Right => self.x = x1 - 1.0,
                    Edge::Top => self.y = y0,
                    Edge::Bottom => self.y = y1 - 1.0,
                }
                self.returns.push((self.x, self.y));
                return true;
            }
            false
        }

        pub fn feed(&mut self, deltas: &[(f64, f64)]) -> usize {
            deltas
                .iter()
                .filter(|(dx, dy)| self.apply(*dx, *dy))
                .count()
        }
    }

    /// Translate a receiver-space motion into capture deltas for the given
    /// entry edge: `outward` is a reverse push (towards leaving the receiver),
    /// `inward` carries the remote cursor into the screen.
    pub fn capture_delta(edge: Edge, outward: f64, inward: f64) -> (f64, f64) {
        match edge {
            Edge::Left => (inward - outward, 0.0),
            Edge::Right => (outward - inward, 0.0),
            Edge::Top => (0.0, inward - outward),
            Edge::Bottom => (0.0, outward - inward),
        }
    }
}

/// The pre-fix (`8b6981d`) source-side residual filter, modelled independently:
/// it dropped leading reverse deltas until the first forward delta, then
/// disarmed, so a backswing arriving *after* the crossing motion leaked to the
/// receiver. Used only as a sensitivity check that the old-receiver model and
/// the trace really do reproduce the reported churn.
fn legacy_forwarded_trace(pos: input_capture::Position, raw: &[(f64, f64)]) -> Vec<(f64, f64)> {
    const BUDGET: f64 = 32.0;
    let mut remaining = Some(BUDGET);
    let mut forwarded = Vec::new();
    for &(dx, dy) in raw {
        if let Some(left) = remaining {
            let outward = match pos {
                input_capture::Position::Left => dx,
                input_capture::Position::Right => -dx,
                input_capture::Position::Top => dy,
                input_capture::Position::Bottom => -dy,
            };
            if outward <= 0.0 {
                remaining = None;
            } else if outward < left {
                remaining = Some(left - outward);
                continue;
            } else {
                remaining = None;
            }
        }
        forwarded.push((dx, dy));
    }
    forwarded
}

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

use old_receiver::{Edge, OldWindowsReceiver};

const HANDLE: input_capture::CaptureHandle = 7;
/// The reported topology: the pointer leaves the Ubuntu machine through its
/// right edge and enters the Windows machine through its left edge.
const SOURCE_POS: input_capture::Position = input_capture::Position::Right;

/// Trace-shaped deltas for the reported churn, in source coordinates for
/// [`SOURCE_POS`]: compositor warp + touchpad backswing (reverse, negative),
/// the crossing motion (forward, positive), then the second backswing that
/// arrives after the first forward delta, then continued forward motion.
const RAW_TRACE: [(f64, f64); 5] = [
    (-2.0, 0.0),
    (-5.0, 0.0),
    (3.0, 0.0),
    (-17.0, 0.0),
    (4.0, 0.0),
];

async fn dummy_capture() -> input_capture::InputCapture {
    input_capture::InputCapture::new(Some(input_capture::Backend::Dummy))
        .await
        .expect("dummy capture")
}

/// Stable, self-deleting temporary directory (same pattern as the existing QA
/// tests; never uses `HOME`/XDG globals, safe for parallel test binaries).
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "lan-mouse-qa-old-receiver-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }

    fn join(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Old-receiver model checks (independent of the production source)
// ---------------------------------------------------------------------------

/// Sensitivity check: if the source forwards the post-forward backswing the
/// way the pre-fix guard did, the v0.2.18 receiver returns control. Without
/// this the replay test below could pass for the wrong reason.
#[test]
fn the_reported_trace_still_triggers_the_old_receiver_when_the_guard_is_absent() {
    let forwarded = legacy_forwarded_trace(SOURCE_POS, &RAW_TRACE);
    assert_eq!(
        forwarded,
        vec![(3.0, 0.0), (-17.0, 0.0), (4.0, 0.0)],
        "the pre-fix source forwarded the first crossing motion and then the backswing"
    );

    let mut receiver =
        OldWindowsReceiver::new(OldWindowsReceiver::edge_for_capture_pos(SOURCE_POS));
    let returns = receiver.feed(&forwarded);
    assert_eq!(
        returns, 1,
        "the old receiver must return control on the leaked backswing: {:?}",
        receiver.returns
    );
}

/// The exact v0.2.18 threshold: a single outward push beyond 3 px past the
/// entry edge fires; a smaller push does not. The anchor is the edge pixel.
#[test]
fn the_old_receiver_margin_is_three_pixels_and_relative_to_the_entry_anchor() {
    let edge = Edge::Left;
    let mut small = OldWindowsReceiver::new(edge);
    assert!(!small.apply(0.0, 0.0));
    assert!(
        !small.apply(-3.0, 0.0),
        "exactly the margin is not past it (strict comparison in v0.2.18)"
    );
    let mut past = OldWindowsReceiver::new(edge);
    assert!(past.apply(-3.5, 0.0));
    assert_eq!(past.returns.len(), 1);
}

/// A forward (inward) delta moves the old receiver's tracked cursor into the
/// screen immediately and never triggers a return, regardless of entry edge.
#[test]
fn a_first_forward_delta_moves_every_old_receiver_edge_inward_without_returning() {
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        let mut receiver = OldWindowsReceiver::new(edge);
        let (dx, dy) = old_receiver::capture_delta(edge, 0.0, 2.0);
        assert!(!receiver.apply(dx, dy), "{edge:?}: forward must not return");
        assert!(receiver.returns.is_empty(), "{edge:?}");
        // The cursor moved 2 px into the screen along the crossing axis.
        let moved = match edge {
            Edge::Left => receiver.x - old_receiver::REGION.0,
            Edge::Right => old_receiver::REGION.2 - 1.0 - receiver.x,
            Edge::Top => receiver.y - old_receiver::REGION.1,
            Edge::Bottom => old_receiver::REGION.3 - 1.0 - receiver.y,
        };
        assert!(
            (moved - 2.0).abs() < 1e-9,
            "{edge:?}: the first forward motion must move the cursor immediately, moved={moved}"
        );
    }
}

/// Sentinel: the fix must stay a bounded, reverse-only filter. A timed settle
/// that drops *all* motion (the old `ENTER_MOTION_SETTLE` regression) or an
/// unbounded suppression would make the report pass but the cursor stick.
#[test]
fn the_handoff_guard_is_bounded_and_reverse_only() {
    let source = include_str!("../src/capture.rs");
    assert!(
        !source.contains("ENTER_MOTION_SETTLE"),
        "a timed all-motion settle must not return: it is the reported stutter"
    );

    assert!(
        source.contains("reverse_guard_until"),
        "the reverse guard must stay deadline-based (fixed window from Begin)"
    );

    let window_ms: u64 = source
        .split_once("HANDOFF_REVERSE_GUARD: Duration = Duration::from_millis(")
        .and_then(|(_, rest)| rest.split(')').next())
        .and_then(|value| value.trim().parse().ok())
        .expect("HANDOFF_REVERSE_GUARD must stay a named millisecond constant");
    assert!(
        window_ms > 0 && window_ms <= 300,
        "the reverse guard window must be short and bounded (got {window_ms} ms)"
    );
}

// ---------------------------------------------------------------------------
// Cursor hide: the source-side hide contract on a stable capture and shutdown
// ---------------------------------------------------------------------------

/// While a source capture is live the heartbeat file must stay fresh; after
/// the capture ends the heartbeat survives the short border-flap grace, and
/// shutdown removes both the heartbeat and the park marker.
#[test]
fn a_stable_capture_keeps_the_hide_and_shutdown_removes_both_markers() {
    let dir = TempDir::new("cursor-hide");
    let hide_path = dir.join("cursor-hide");
    let park_path = dir.join("cursor-park");

    let mut hide = cursor_hide::CursorHide::with_path(Some(hide_path.clone()));
    hide.enter(7);
    assert!(
        hide_path.exists(),
        "an outgoing capture must hide the cursor"
    );
    assert!(hide.is_outgoing_active());

    // The service ticks the heartbeat once per HEARTBEAT_INTERVAL; it must
    // stay present (the extension only hides while the mtime is fresh).
    hide.tick();
    assert!(
        hide_path.exists(),
        "a stable capture must keep the heartbeat file present"
    );

    // A park marker left at the edge must never coexist with a live capture.
    hide.park();
    assert!(
        !park_path.exists(),
        "parking must not shortcut an outgoing capture"
    );

    hide.leave(7);
    assert!(
        hide_path.exists(),
        "the heartbeat must survive the post-leave grace to avoid a border flash"
    );
    assert!(!hide.is_outgoing_active());

    // Shutdown/close: both keep-hiding artefacts must go before the (possibly
    // failing) backend/portal teardown.
    {
        let mut hide = cursor_hide::CursorHide::with_path(Some(hide_path.clone()));
        hide.park();
        assert!(
            park_path.exists(),
            "a returned cursor is parked at the edge"
        );
        hide.shutdown();
        assert!(
            !hide_path.exists() && !park_path.exists(),
            "shutdown must leave no cursor-hide/cursor-park artefact behind"
        );
    }

    // Dropping the state (every exit path) must be equally safe.
    {
        let mut hide = cursor_hide::CursorHide::with_path(Some(hide_path.clone()));
        hide.enter(9);
        hide.park();
    }
    assert!(
        !hide_path.exists() && !park_path.exists(),
        "dropping the cursor state must restore the cursor"
    );
}
