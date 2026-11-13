//! Independent bidirectional hand-off verification (task-11).
//!
//! Reported regression (Ubuntu on current `main`, Windows on old 0.2.18):
//! the cursor churns in **both** directions —
//!
//! - Ubuntu source -> Windows 0.2.18 receiver: Ubuntu enters the client, then
//!   receives `LeaveAt` roughly every 1-2 s and leaves again;
//! - Windows 0.2.18 source -> Ubuntu receiver: the Ubuntu receiver logs
//!   repeated `handoff: return request ...` (edge returns and `EnterOnly`
//!   re-entry `Leave`s) while Windows is still controlling.
//!
//! task-9 established two independent causes that a same-build 0.2.19 alone
//! does not remove:
//!
//! 1. the source guard is **time-only** (fixed 200 ms): a late reverse
//!    residual / activation warp that arrives after the window is forwarded
//!    to a receiver that fires from ~1-3 px past the entry edge;
//! 2. the receiver-side `EnterOnly` capture re-entry loop: a jittery
//!    `CaptureBegin` while a remote peer already drives the screen turns into
//!    `request_handoff(addr, None)` and the peer releases/re-enters.
//!
//! This harness independently verifies the final fix:
//!
//! - **real-source replay**: `src/capture.rs` is compiled read-only into this
//!   test crate (`mod capture { include!(...) }`) and its private
//!   `CaptureTask` state machine is driven with trace-shaped deltas for both
//!   directions; the deltas it actually forwards feed two independently
//!   written receiver models (`NewUbuntuReceiver`: `main` libei tracker,
//!   `OldWindowsReceiver0218`: release 0.2.18 Windows backend tracker);
//! - **unguarded-source sensitivity checks** prove the mixed-build churn is
//!   reproducible (a 0.2.18 source forwards activation residuals, the
//!   receivers fire);
//! - **receiver re-entry contract**: repeated jittery `EnterOnly` begins
//!   while a remote peer drives the screen must not generate a `Leave` loop,
//!   while a deliberate local grab still works;
//! - cursor hide/park/shutdown are re-checked from the shipped
//!   `src/cursor_hide.rs` (the same compiled-source technique).
//!
//! Layer L2 deterministic contracts: no sockets, no sleeps, no display,
//! virtual timestamps only.

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
// The shipping capture task: real source state machine under test.
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
        let (task, sent, _events) = replay_task_with_events(captures);
        (task, sent)
    }

    /// Same, but also exposes the `ICaptureEvent` channel (the service uses
    /// `CaptureBegin` to request a return hand-off from an incoming peer).
    fn replay_task_with_events(
        captures: Vec<(CaptureHandle, Position, CaptureType)>,
    ) -> (
        CaptureTask,
        Rc<RefCell<Vec<ProtoEvent>>>,
        Receiver<ICaptureEvent>,
    ) {
        let (event_tx, event_rx) = channel();
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
        (task, sent, event_rx)
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

    /// Simulate only the old fixed 200 ms telemetry window elapsing: the
    /// bounded position-aware filter (250 ms) is still active, which is the
    /// 204.0-205.4 ms settling cluster from the trace.
    fn replay_expire_fixed_window_only(task: &mut CaptureTask) {
        assert!(
            task.handoff_inward_px.is_some(),
            "precondition: the position-aware filter must be armed after Begin"
        );
        task.reverse_guard_until = Some(Instant::now() - Duration::from_millis(1));
    }

    /// Simulate both guard windows elapsing without sleeping: the fixed 200 ms
    /// telemetry window and the bounded position-aware filter.
    fn replay_expire_reverse_guard(task: &mut CaptureTask) {
        assert!(
            task.handoff_inward_px.is_some(),
            "precondition: the position-aware filter must be armed after Begin"
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

    // -----------------------------------------------------------------
    // Bidirectional replay tests (module scope: they need `CaptureTask`).
    // -----------------------------------------------------------------

    /// The first forward delta of a crossing must be forwarded in the same
    /// handler call, unchanged, in **both** directions: no settle window, no
    /// rescale, no ack dependency. This is the "no force" contract.
    #[tokio::test(flavor = "current_thread")]
    async fn the_first_forward_delta_is_forwarded_immediately_in_both_directions() {
        for pos in [crate::UBUNTU_SOURCE_POS, crate::WINDOWS_SOURCE_POS] {
            let (mut task, sent) = replay_task(vec![(crate::HANDLE, pos, CaptureType::Default)]);
            let mut input_capture = crate::dummy_capture().await;
            replay_begin_at(&mut task, &mut input_capture, crate::HANDLE, 0.5).await;
            let forward = crate::forward_delta(pos, 2.0);
            replay_motion(
                &mut task,
                &mut input_capture,
                crate::HANDLE,
                forward.0,
                forward.1,
            )
            .await;

            assert_eq!(
                forwarded_motions(&sent.borrow()),
                vec![forward],
                "{pos:?}: the first forward motion must be forwarded at once and unchanged"
            );
            input_capture.terminate().await.expect("terminate");
        }
    }

    /// Same-build Windows -> Ubuntu: the guarded source suppresses the
    /// activation residual, so the (very sensitive) current Ubuntu receiver
    /// sees only inward motion and never asks for the capture back.
    #[tokio::test(flavor = "current_thread")]
    async fn a_guarded_windows_source_keeps_the_new_ubuntu_receiver_quiet() {
        let (mut task, sent) = replay_task(vec![(
            crate::HANDLE,
            crate::WINDOWS_SOURCE_POS,
            CaptureType::Default,
        )]);
        let mut input_capture = crate::dummy_capture().await;
        replay_begin_at(&mut task, &mut input_capture, crate::HANDLE, 0.5).await;
        replay_ack(&mut task);
        for (dx, dy) in crate::WINDOWS_RAW {
            replay_motion(&mut task, &mut input_capture, crate::HANDLE, dx, dy).await;
        }

        let forwarded = forwarded_motions(&sent.borrow());
        let expected: Vec<(f64, f64)> = crate::WINDOWS_RAW
            .iter()
            .filter(|(dx, _)| *dx < 0.0)
            .copied()
            .collect();
        assert_eq!(
            forwarded, expected,
            "only the crossing motion may reach the receiver, got {forwarded:?}"
        );

        let mut receiver =
            crate::new_receiver::NewUbuntuReceiver::new(crate::old_receiver::Edge::Right);
        assert_eq!(
            receiver.feed(&forwarded),
            0,
            "the new Ubuntu receiver must not request a return: {:?}",
            receiver.returns
        );
        // The cursor moved into the screen (away from the Right edge).
        assert!(
            (receiver.x - (1919.0 - 7.0)).abs() < 1e-9,
            "the forwarded motion must move the remote cursor inward, x={}",
            receiver.x
        );

        input_capture.terminate().await.expect("terminate");
    }

    /// Sensitivity check for the Windows -> Ubuntu direction: the old 0.2.18
    /// source has no reverse guard (`v0.2.18:src/capture.rs` contains none),
    /// so its activation residual can reach the current receiver after the
    /// entrance settle - exactly the reported `pushed past the Right edge`
    /// churn. The task-10 receiver deadband must still ignore the *immediate*
    /// entrance residual (that half is what the deadband fixes), while a late
    /// residual after the pointer was used inside the screen remains a real
    /// return push (so the deadband is not a blanket return blocker).
    #[test]
    fn an_unguarded_0218_windows_source_triggers_the_new_ubuntu_receiver_after_settling() {
        // `EnterAt` anchor.
        let mut receiver =
            crate::new_receiver::NewUbuntuReceiver::new(crate::old_receiver::Edge::Right);
        // Entrance residual inside the settle window is ignored by task-10.
        assert_eq!(
            receiver.feed(&[(2.0, 0.0), (5.0, 0.0)]),
            0,
            "immediate entrance residual must be ignored by the receiver settle/deadband"
        );
        // The user works inside the screen for a moment...
        assert_eq!(receiver.feed(&[(-3.0, 0.0)]), 0);
        receiver.advance_ms(crate::new_receiver::ENTRY_RETURN_SETTLE_MS);
        // ...then the old unguarded source's late backswing arrives outward.
        assert!(
            receiver.feed(&[(17.0, 0.0)]) >= 1,
            "a late residual after the settle must reproduce the receiver return request"
        );

        let mut legacy =
            crate::new_receiver::NewUbuntuReceiver::legacy(crate::old_receiver::Edge::Right);
        assert_eq!(legacy.feed(&[(2.0, 0.0), (5.0, 0.0)]), 0);
        assert_eq!(legacy.feed(&[(-3.0, 0.0)]), 0);
        legacy.advance_ms(crate::new_receiver::ENTRY_RETURN_SETTLE_MS);
        assert!(
            legacy.feed(&[(17.0, 0.0)]) >= 1,
            "the legacy along=None anchor must also reproduce the churn"
        );
    }

    /// Same-build Ubuntu -> Windows: the guarded source keeps the old 0.2.18
    /// Windows receiver quiet (the task-6 guarantee, re-checked from the
    /// bidirectional harness with the actual Windows-backend model).
    #[tokio::test(flavor = "current_thread")]
    async fn a_guarded_ubuntu_source_keeps_the_old_windows_receiver_quiet() {
        let (mut task, sent) = replay_task(vec![(
            crate::HANDLE,
            crate::UBUNTU_SOURCE_POS,
            CaptureType::Default,
        )]);
        let mut input_capture = crate::dummy_capture().await;
        replay_begin_at(&mut task, &mut input_capture, crate::HANDLE, 0.5).await;
        replay_ack(&mut task);
        for (dx, dy) in crate::UBUNTU_RAW {
            replay_motion(&mut task, &mut input_capture, crate::HANDLE, dx, dy).await;
        }

        let forwarded = forwarded_motions(&sent.borrow());
        let expected: Vec<(f64, f64)> = crate::UBUNTU_RAW
            .iter()
            .filter(|(dx, _)| *dx > 0.0)
            .copied()
            .collect();
        assert_eq!(forwarded, expected, "got {forwarded:?}");

        let mut receiver = crate::old_windows_receiver::OldWindowsReceiver0218::new(
            crate::old_receiver::Edge::Left,
        );
        assert_eq!(
            receiver.feed(&forwarded),
            0,
            "the old Windows receiver must not ask for control back"
        );
        assert_eq!(
            receiver.x, 8,
            "the forward deltas must move the pinned Windows cursor inward"
        );

        input_capture.terminate().await.expect("terminate");
    }

    /// Sensitivity check for the Ubuntu -> Windows direction: without the
    /// source guard the activation residual reaches the old Windows receiver,
    /// which parks 1 px inside the edge and returns from a 2-px overshoot.
    #[test]
    fn an_unguarded_source_triggers_the_old_windows_receiver() {
        let mut receiver = crate::old_windows_receiver::OldWindowsReceiver0218::new(
            crate::old_receiver::Edge::Left,
        );
        assert!(
            receiver.feed(&crate::UBUNTU_RAW) >= 1,
            "the unguarded backswing must reproduce the old receiver LeaveAt"
        );
    }

    /// A deliberate reverse gesture must still be delivered after the guard
    /// window in both directions: the no-force contract must not turn into a
    /// no-return contract.
    #[tokio::test(flavor = "current_thread")]
    async fn a_genuine_return_is_still_delivered_in_both_directions() {
        // Ubuntu -> Windows: +3 inward, then a real -8 return after the window.
        let (mut task, sent) = replay_task(vec![(
            crate::HANDLE,
            crate::UBUNTU_SOURCE_POS,
            CaptureType::Default,
        )]);
        let mut input_capture = crate::dummy_capture().await;
        replay_begin_at(&mut task, &mut input_capture, crate::HANDLE, 0.5).await;
        replay_ack(&mut task);
        replay_motion(&mut task, &mut input_capture, crate::HANDLE, 3.0, 0.0).await;
        replay_expire_reverse_guard(&mut task);
        replay_motion(&mut task, &mut input_capture, crate::HANDLE, -8.0, 0.0).await;
        let forwarded = forwarded_motions(&sent.borrow());
        assert_eq!(forwarded, vec![(3.0, 0.0), (-8.0, 0.0)]);
        let mut old_receiver = crate::old_windows_receiver::OldWindowsReceiver0218::new(
            crate::old_receiver::Edge::Left,
        );
        assert_eq!(
            old_receiver.feed(&forwarded),
            1,
            "the return must hand back"
        );
        input_capture.terminate().await.expect("terminate");

        // Windows -> Ubuntu: -3 inward, then a real +8 return after the window.
        let (mut task, sent) = replay_task(vec![(
            crate::HANDLE,
            crate::WINDOWS_SOURCE_POS,
            CaptureType::Default,
        )]);
        let mut input_capture = crate::dummy_capture().await;
        replay_begin_at(&mut task, &mut input_capture, crate::HANDLE, 0.5).await;
        replay_ack(&mut task);
        replay_motion(&mut task, &mut input_capture, crate::HANDLE, -3.0, 0.0).await;
        replay_expire_reverse_guard(&mut task);
        replay_motion(&mut task, &mut input_capture, crate::HANDLE, 8.0, 0.0).await;
        let forwarded = forwarded_motions(&sent.borrow());
        assert_eq!(forwarded, vec![(-3.0, 0.0), (8.0, 0.0)]);
        // The receiver accepts a return only after the entrance settle (the
        // crossing motion above is the required "work inside the screen").
        let mut new_receiver =
            crate::new_receiver::NewUbuntuReceiver::new(crate::old_receiver::Edge::Right);
        assert_eq!(new_receiver.feed(&forwarded[..1]), 0);
        new_receiver.advance_ms(crate::new_receiver::ENTRY_RETURN_SETTLE_MS);
        assert_eq!(
            new_receiver.feed(&forwarded[1..]),
            1,
            "the return must hand back"
        );
        input_capture.terminate().await.expect("terminate");
    }

    /// A reverse residual that arrives **after** the old fixed 200 ms window
    /// while the pointer is still next to the entry edge is the ranked cause
    /// #1 of the bidirectional analysis (the hand-off settling first-motion
    /// samples cluster at 204.0-205.4 ms). The final position-aware filter must
    /// suppress it in both directions; the cursor cannot have moved "clearly
    /// inward" after a couple of pixels, so this is still activation residual.
    #[tokio::test(flavor = "current_thread")]
    async fn a_late_reverse_residual_near_the_entry_edge_is_suppressed() {
        for pos in [crate::UBUNTU_SOURCE_POS, crate::WINDOWS_SOURCE_POS] {
            let (mut task, sent) = replay_task(vec![(crate::HANDLE, pos, CaptureType::Default)]);
            let mut input_capture = crate::dummy_capture().await;
            replay_begin_at(&mut task, &mut input_capture, crate::HANDLE, 0.5).await;
            replay_ack(&mut task);
            // A small crossing motion, then the window expires...
            let forward = crate::forward_delta(pos, 3.0);
            replay_motion(
                &mut task,
                &mut input_capture,
                crate::HANDLE,
                forward.0,
                forward.1,
            )
            .await;
            replay_expire_fixed_window_only(&mut task);
            // ...and a ~17 px activation warp arrives while still at the edge.
            let late = crate::reverse_delta(pos, 17.0);
            replay_motion(&mut task, &mut input_capture, crate::HANDLE, late.0, late.1).await;

            let forwarded = forwarded_motions(&sent.borrow());
            assert_eq!(
                forwarded,
                vec![forward],
                "{pos:?}: a late activation warp next to the edge must not be forwarded \
                 (got {forwarded:?})"
            );
            input_capture.terminate().await.expect("terminate");
        }
    }

    /// Once the remote cursor has moved clearly inside the screen, reverse
    /// motion is user intent and must be forwarded immediately (no extra
    /// force): the position-aware filter must not become a return blocker.
    #[tokio::test(flavor = "current_thread")]
    async fn a_reverse_gesture_after_moving_inward_is_forwarded() {
        for pos in [crate::UBUNTU_SOURCE_POS, crate::WINDOWS_SOURCE_POS] {
            let (mut task, sent) = replay_task(vec![(crate::HANDLE, pos, CaptureType::Default)]);
            let mut input_capture = crate::dummy_capture().await;
            replay_begin_at(&mut task, &mut input_capture, crate::HANDLE, 0.5).await;
            replay_ack(&mut task);
            // Move clearly into the screen in several forward steps.
            for _ in 0..5 {
                let forward = crate::forward_delta(pos, 12.0);
                replay_motion(
                    &mut task,
                    &mut input_capture,
                    crate::HANDLE,
                    forward.0,
                    forward.1,
                )
                .await;
            }
            // Now a deliberate reverse gesture.
            let reverse = crate::reverse_delta(pos, 10.0);
            replay_motion(
                &mut task,
                &mut input_capture,
                crate::HANDLE,
                reverse.0,
                reverse.1,
            )
            .await;

            let forwarded = forwarded_motions(&sent.borrow());
            assert_eq!(
                forwarded.last().copied(),
                Some(reverse),
                "{pos:?}: a deliberate return after moving inward must be forwarded"
            );
            input_capture.terminate().await.expect("terminate");
        }
    }

    /// The bounded position-aware filter is a safety escape, not a trap: a
    /// held reverse gesture next to the edge still passes once the 250 ms
    /// bound expires (worst case 250 ms delay, never a lost hand-back), while
    /// forward motion was forwarded normally all along.
    #[tokio::test(flavor = "current_thread")]
    async fn the_position_guard_is_bounded_so_a_held_reverse_gesture_passes() {
        for pos in [crate::UBUNTU_SOURCE_POS, crate::WINDOWS_SOURCE_POS] {
            let (mut task, sent) = replay_task(vec![(crate::HANDLE, pos, CaptureType::Default)]);
            let mut input_capture = crate::dummy_capture().await;
            replay_begin_at(&mut task, &mut input_capture, crate::HANDLE, 0.5).await;
            replay_ack(&mut task);
            let forward = crate::forward_delta(pos, 3.0);
            replay_motion(
                &mut task,
                &mut input_capture,
                crate::HANDLE,
                forward.0,
                forward.1,
            )
            .await;
            // Both deadlines expire while the pointer is still next to the edge.
            replay_expire_reverse_guard(&mut task);
            let reverse = crate::reverse_delta(pos, 5.0);
            replay_motion(
                &mut task,
                &mut input_capture,
                crate::HANDLE,
                reverse.0,
                reverse.1,
            )
            .await;

            assert_eq!(
                forwarded_motions(&sent.borrow()),
                vec![forward, reverse],
                "{pos:?}: after the bounded filter expires, a held reverse gesture must pass"
            );
            input_capture.terminate().await.expect("terminate");
        }
    }

    /// Task-7 regression under the position-aware filter: a large activation
    /// burst (serial-2 scene, 34.9 px reverse) is still suppressed while the
    /// receiver cursor is next to the edge, and the old receiver does not fire.
    #[tokio::test(flavor = "current_thread")]
    async fn large_activation_bursts_are_still_suppressed_near_the_edge() {
        let raw = [(-17.961, 0.0), (12.390, 0.0), (-16.938, 0.0)];
        let (mut task, sent) = replay_task(vec![(
            crate::HANDLE,
            crate::UBUNTU_SOURCE_POS,
            CaptureType::Default,
        )]);
        let mut input_capture = crate::dummy_capture().await;
        replay_begin_at(&mut task, &mut input_capture, crate::HANDLE, 0.5).await;
        replay_ack(&mut task);
        for (dx, dy) in raw {
            replay_motion(&mut task, &mut input_capture, crate::HANDLE, dx, dy).await;
        }
        let forwarded = forwarded_motions(&sent.borrow());
        assert_eq!(
            forwarded,
            vec![(12.390, 0.0)],
            "only the forward crossing motion may be forwarded, got {forwarded:?}"
        );
        let mut receiver = crate::old_windows_receiver::OldWindowsReceiver0218::new(
            crate::old_receiver::Edge::Left,
        );
        assert_eq!(receiver.feed(&forwarded), 0);
        input_capture.terminate().await.expect("terminate");
    }

    /// Receiver re-entry loop (task-9 cause #2): while a remote peer drives
    /// this screen, jittery `Begin` events on the incoming `EnterOnly`
    /// capture must not be forwarded to the service as
    /// `ICaptureEvent::CaptureBegin` — the service turns each of those into
    /// `request_handoff(addr, None)`, the peer releases, the injected pointer
    /// is parked on the edge and re-arms the barrier: the reported
    /// `handoff: return request ... along=None` storm.
    #[tokio::test(flavor = "current_thread")]
    async fn jittery_enter_only_begins_while_remote_drives_do_not_loop() {
        use futures::FutureExt;

        let (mut task, _sent, mut events) = replay_task_with_events(vec![
            (crate::HANDLE, Position::Right, CaptureType::Default),
            (crate::HANDLE + 1, Position::Right, CaptureType::EnterOnly),
        ]);
        // A peer entered through the shared Right edge and is driving us.
        task.update_remote_peer(Some(lan_mouse_ipc::Position::Right));
        let mut input_capture = crate::dummy_capture().await;

        for _ in 0..8 {
            task.handle_capture_event(
                &mut input_capture,
                (crate::HANDLE + 1, CaptureEvent::BeginAt(0.5)),
            )
            .await
            .expect("jittery enter-only begin");
        }

        assert!(
            events.recv().now_or_never().is_none(),
            "a jittery EnterOnly begin while a remote peer drives must not emit CaptureBegin \
             (that is the request_handoff(addr, None) re-entry loop)"
        );
        input_capture.terminate().await.expect("terminate");
    }

    /// After the entrance dwell, a genuine local take-back is reported exactly
    /// once per remote-driving session (the service retries the request); the
    /// gate must not turn into a blanket "never take control back".
    #[tokio::test(flavor = "current_thread")]
    async fn a_deliberate_take_back_after_the_dwell_is_reported_once_per_session() {
        use futures::FutureExt;

        let (mut task, _sent, mut events) = replay_task_with_events(vec![
            (crate::HANDLE, Position::Right, CaptureType::Default),
            (crate::HANDLE + 1, Position::Right, CaptureType::EnterOnly),
        ]);
        task.update_remote_peer(Some(lan_mouse_ipc::Position::Right));
        // Simulate the dwell elapsing without sleeping.
        task.remote_peer_since = Some(Instant::now() - Duration::from_millis(500));
        let mut input_capture = crate::dummy_capture().await;

        task.handle_capture_event(
            &mut input_capture,
            (crate::HANDLE + 1, CaptureEvent::BeginAt(0.5)),
        )
        .await
        .expect("take-back begin");
        assert!(
            events.recv().now_or_never().is_some(),
            "a deliberate local take-back after the dwell must be reported"
        );

        // The service retries; a second begin in the same session is noise.
        task.handle_capture_event(
            &mut input_capture,
            (crate::HANDLE + 1, CaptureEvent::BeginAt(0.5)),
        )
        .await
        .expect("repeated take-back begin");
        assert!(
            events.recv().now_or_never().is_none(),
            "only one take-back request per remote-driving session"
        );
        input_capture.terminate().await.expect("terminate");
    }

    /// The re-entry gate must be scoped to the remote-driving case: with no
    /// remote peer driving, the incoming `EnterOnly` begin keeps its old
    /// service notification contract (the local user entering the shared zone
    /// still triggers the hand-off), so this is not a blanket suppression.
    #[tokio::test(flavor = "current_thread")]
    async fn an_enter_only_begin_without_a_remote_peer_still_notifies_the_service() {
        use futures::FutureExt;

        let (mut task, _sent, mut events) = replay_task_with_events(vec![
            (crate::HANDLE, Position::Right, CaptureType::Default),
            (crate::HANDLE + 1, Position::Right, CaptureType::EnterOnly),
        ]);
        assert!(task.remote_peer.is_none());
        let mut input_capture = crate::dummy_capture().await;

        task.handle_capture_event(
            &mut input_capture,
            (crate::HANDLE + 1, CaptureEvent::BeginAt(0.5)),
        )
        .await
        .expect("enter-only begin");

        assert!(
            events.recv().now_or_never().is_some(),
            "without a remote peer driving, the EnterOnly begin must still emit CaptureBegin"
        );
        input_capture.terminate().await.expect("terminate");
    }

    // -----------------------------------------------------------------
    // Protocol / capability contract for the same-build candidate.
    // -----------------------------------------------------------------

    /// The hand-off capabilities are built in and unchanged between `v0.2.18`
    /// and `main`; a same-build pair negotiates all of them, which is what
    /// makes `EnterAt`/`LeaveAt`/serialized `Leave`/version checks symmetric.
    /// This pins the capability set so a future protocol edit cannot silently
    /// downgrade one direction to the legacy fire-and-forget path.
    #[test]
    fn same_build_negotiates_bidirectional_handoff_capabilities() {
        let proto = include_str!("../lan-mouse-proto/src/lib.rs");
        let caps = proto
            .split("pub const CAPABILITIES: u32 =")
            .nth(1)
            .expect("CAPABILITIES block")
            .split(';')
            .next()
            .expect("CAPABILITIES value");
        for capability in [
            "ENTER_ACK",
            "SERIALIZED_LEAVE",
            "EDGE_COORDINATE",
            "RETURN_COORDINATE",
            "VERSION_INFO",
        ] {
            assert!(
                caps.contains(capability),
                "CAPABILITIES must contain {capability}: {caps}"
            );
        }
        // Both directions use the same two wire events.
        assert!(proto.contains("EnterAt"));
        assert!(proto.contains("LeaveAt"));
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
// Independent model of the CURRENT Ubuntu/Linux receiver, including the
// task-10 receiver-side return deadband.
//
// Transcribed from `input-emulation/src/libei.rs::AbsoluteTracker`:
// `RETURN_MARGIN_PX = 0.0`, `EDGE_ANCHOR_INSET = 2.0` for the legacy
// (no-coordinate) `Enter`, exact entry-edge anchor for `EnterAt`, an outward
// direction check, a 20 px re-arm inset after a return, and the task-10
// return gates `ENTRY_WORK_DEADBAND_PX = 2.0` + `ENTRY_RETURN_SETTLE = 250 ms`
// (an outward push is only a real return after the pointer has actually been
// used inside the screen and the entrance settle has elapsed). This is the
// endpoint that generated the repeated `pushed past the Right edge` requests
// while the old Windows source was controlling.
// ---------------------------------------------------------------------------

mod new_receiver {
    use super::old_receiver::{Edge, REGION};

    /// No extra margin: the anchor is 1-2 px inside the edge, so one outward
    /// pixel can hand control back (`8aebfdf` deliberately made this tiny).
    pub const RETURN_MARGIN_PX: f64 = 0.0;
    /// Legacy `Enter` (no crossing coordinate) anchor inset.
    pub const EDGE_ANCHOR_INSET: f64 = 2.0;
    /// Re-arm inset after a return.
    pub const REARM_INSET_PX: f64 = 20.0;
    /// Inward displacement the pointer must reach before a return is accepted.
    pub const ENTRY_WORK_DEADBAND_PX: f64 = 2.0;
    /// Entrance settle before a return is accepted.
    pub const ENTRY_RETURN_SETTLE_MS: f64 = 250.0;

    #[derive(Clone, Debug)]
    pub struct NewUbuntuReceiver {
        pub edge: Edge,
        pub x: f64,
        pub y: f64,
        pub fired: bool,
        pub returns: Vec<(f64, f64)>,
        /// Virtual time in ms since arming.
        pub now_ms: f64,
        /// Deepest inward displacement reached since arming.
        pub inward_travel_px: f64,
    }

    impl NewUbuntuReceiver {
        /// `EnterAt(along)`: anchor exactly on the entry-edge pixel.
        pub fn new(edge: Edge) -> Self {
            let (x0, y0, x1, y1) = REGION;
            let (x, y) = match edge {
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
                now_ms: 0.0,
                inward_travel_px: 0.0,
            }
        }

        /// Legacy `Enter` (the 0.2.18 peer's `along=None` fallback): the
        /// pointer is parked `EDGE_ANCHOR_INSET` px inside the edge.
        pub fn legacy(edge: Edge) -> Self {
            let (x0, y0, x1, y1) = REGION;
            let (x, y) = match edge {
                Edge::Left => (x0 + EDGE_ANCHOR_INSET, (y0 + y1) / 2.0),
                Edge::Right => (x1 - 1.0 - EDGE_ANCHOR_INSET, (y0 + y1) / 2.0),
                Edge::Top => ((x0 + x1) / 2.0, y0 + EDGE_ANCHOR_INSET),
                Edge::Bottom => ((x0 + x1) / 2.0, y1 - 1.0 - EDGE_ANCHOR_INSET),
            };
            Self {
                edge,
                x,
                y,
                fired: false,
                returns: Vec::new(),
                now_ms: 0.0,
                inward_travel_px: 0.0,
            }
        }

        /// Advance the virtual clock (used to cross the entrance settle).
        pub fn advance_ms(&mut self, ms: f64) {
            self.now_ms += ms;
        }

        /// Whether an outward push may be accepted as a deliberate hand-back:
        /// the user must have used the screen (work deadband) and the entrance
        /// settle must have elapsed (`ENTRY_RETURN_SETTLE`).
        fn return_ready(&self) -> bool {
            self.inward_travel_px >= ENTRY_WORK_DEADBAND_PX && self.now_ms >= ENTRY_RETURN_SETTLE_MS
        }

        /// Inward distance of `(x, y)` from the entry edge (positive inside).
        fn inward_distance(&self, x: f64, y: f64) -> f64 {
            let (x0, y0, x1, y1) = REGION;
            match self.edge {
                Edge::Left => x - x0,
                Edge::Right => x1 - 1.0 - x,
                Edge::Top => y - y0,
                Edge::Bottom => y1 - 1.0 - y,
            }
        }

        pub fn apply(&mut self, dx: f64, dy: f64) -> bool {
            let (x0, y0, x1, y1) = REGION;
            let raw_x = self.x + dx;
            let raw_y = self.y + dy;
            let x = raw_x.clamp(x0, x1 - 1.0);
            let y = raw_y.clamp(y0, y1 - 1.0);
            self.inward_travel_px = self.inward_travel_px.max(self.inward_distance(x, y));

            if self.fired {
                self.x = x;
                self.y = y;
                if self.x > x0 + REARM_INSET_PX
                    && self.x < x1 - REARM_INSET_PX
                    && self.y > y0 + REARM_INSET_PX
                    && self.y < y1 - REARM_INSET_PX
                {
                    self.fired = false;
                }
                return false;
            }

            let near_edge = match self.edge {
                Edge::Left => x <= x0 + 1.0,
                Edge::Right => x >= x1 - 2.0,
                Edge::Top => y <= y0 + 1.0,
                Edge::Bottom => y >= y1 - 2.0,
            };
            // `main` requires the injected delta itself to point outward
            // (`past_edge = outward && ...`), unlike 0.2.18.
            let outward = match self.edge {
                Edge::Left => dx < 0.0,
                Edge::Right => dx > 0.0,
                Edge::Top => dy < 0.0,
                Edge::Bottom => dy > 0.0,
            };
            let past_edge = outward
                && match self.edge {
                    Edge::Left => raw_x < x0 + RETURN_MARGIN_PX,
                    Edge::Right => raw_x >= x1 - RETURN_MARGIN_PX,
                    Edge::Top => raw_y < y0 + RETURN_MARGIN_PX,
                    Edge::Bottom => raw_y >= y1 - RETURN_MARGIN_PX,
                };
            self.x = x;
            self.y = y;
            if near_edge && past_edge && self.return_ready() {
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
}

// ---------------------------------------------------------------------------
// Independent model of the OLD Windows 0.2.18 RECEIVER (actual Windows
// backend, `input-emulation/src/windows.rs` + `absolute.rs` @ 5233b05).
//
// * `EnterAt` entry anchor: 1 px inside the entry edge
//   (`entry_anchor_fraction_in_monitors(..., 1)`); legacy `Enter`: 24 px
//   (`ENTRY_ANCHOR_MARGIN`).
// * Return requested when the unclamped target overshoots the entry edge by
//   `>= EDGE_RETURN_MIN_OVERSHOOT = 2` integer physical pixels; only the
//   entry edge counts.
// * The OS clamps the cursor into the virtual screen after every event, and
//   motion is integer physical pixels (no fractional accumulation in the
//   replay corpus, all magnitudes are integers).
// ---------------------------------------------------------------------------

mod old_windows_receiver {
    use super::old_receiver::Edge;

    pub const EDGE_RETURN_MIN_OVERSHOOT: i32 = 2;
    pub const ENTRY_ANCHOR_MARGIN: i32 = 24;
    pub const ENTER_AT_INSET: i32 = 1;
    pub const SCREEN: (i32, i32, i32, i32) = (0, 0, 1920, 1080);

    #[derive(Clone, Debug)]
    pub struct OldWindowsReceiver0218 {
        pub edge: Edge,
        pub x: i32,
        pub y: i32,
        pub fired: bool,
        pub returns: Vec<(i32, i32)>,
    }

    impl OldWindowsReceiver0218 {
        pub fn new(edge: Edge) -> Self {
            Self::at(edge, ENTER_AT_INSET)
        }

        pub fn legacy(edge: Edge) -> Self {
            Self::at(edge, ENTRY_ANCHOR_MARGIN)
        }

        fn at(edge: Edge, margin: i32) -> Self {
            let (x0, y0, x1, y1) = SCREEN;
            let (x, y) = match edge {
                Edge::Left => (x0 + margin, (y0 + y1) / 2),
                Edge::Right => (x1 - 1 - margin, (y0 + y1) / 2),
                Edge::Top => ((x0 + x1) / 2, y0 + margin),
                Edge::Bottom => ((x0 + x1) / 2, y1 - 1 - margin),
            };
            Self {
                edge,
                x,
                y,
                fired: false,
                returns: Vec::new(),
            }
        }

        pub fn apply(&mut self, dx: f64, dy: f64) -> bool {
            let (x0, y0, x1, y1) = SCREEN;
            let (mx, my) = (dx.trunc() as i32, dy.trunc() as i32);
            if mx == 0 && my == 0 {
                return false;
            }
            let requested = (self.x + mx, self.y + my);
            let overshoot = match self.edge {
                Edge::Left => (x0 - requested.0).max(0),
                Edge::Right => (requested.0 - (x1 - 1)).max(0),
                Edge::Top => (y0 - requested.1).max(0),
                Edge::Bottom => (requested.1 - (y1 - 1)).max(0),
            };
            self.x = requested.0.clamp(x0, x1 - 1);
            self.y = requested.1.clamp(y0, y1 - 1);
            if overshoot >= EDGE_RETURN_MIN_OVERSHOOT {
                if self.fired {
                    return false;
                }
                self.fired = true;
                self.returns.push((self.x, self.y));
                return true;
            }
            self.fired = false;
            false
        }

        pub fn feed(&mut self, deltas: &[(f64, f64)]) -> usize {
            deltas
                .iter()
                .filter(|(dx, dy)| self.apply(*dx, *dy))
                .count()
        }
    }
}

// ---------------------------------------------------------------------------
// Test fixtures and trace-shaped scenes.
// ---------------------------------------------------------------------------

const HANDLE: input_capture::CaptureHandle = 7;

/// Desk topology: Ubuntu monitor on the LEFT, Windows monitor on the RIGHT.
///
/// * Ubuntu -> Windows crossing: Ubuntu source `Position::Right`, forward
///   motion is `+dx`; the Windows receiver is entered through its LEFT edge.
/// * Windows -> Ubuntu crossing: Windows source `Position::Left`, forward
///   motion is `-dx`; the Ubuntu receiver is entered through its RIGHT edge.
const UBUNTU_SOURCE_POS: input_capture::Position = input_capture::Position::Right;
const WINDOWS_SOURCE_POS: input_capture::Position = input_capture::Position::Left;

/// Trace-shaped activation sequence for the Ubuntu -> Windows direction
/// (source coordinates): compositor warp/backswing (`-2`, `-5`), the crossing
/// motion (`+3`), the post-forward backswing (`-17`), continued motion (`+4`).
/// Magnitudes come from the local `handoff-trace.jsonl` records cited in
/// `docs/analysis/handoff-bidirectional.md` §4.2.
const UBUNTU_RAW: [(f64, f64); 5] = [
    (-2.0, 0.0),
    (-5.0, 0.0),
    (3.0, 0.0),
    (-17.0, 0.0),
    (4.0, 0.0),
];

/// The same activation sequence seen from a Windows -> Ubuntu crossing (the
/// crossing axis is mirrored, so the residual magnitudes are positive).
const WINDOWS_RAW: [(f64, f64); 5] = [
    (2.0, 0.0),
    (5.0, 0.0),
    (-3.0, 0.0),
    (17.0, 0.0),
    (-4.0, 0.0),
];

async fn dummy_capture() -> input_capture::InputCapture {
    input_capture::InputCapture::new(Some(input_capture::Backend::Dummy))
        .await
        .expect("dummy capture")
}

/// Stable, self-deleting temporary directory (worker-local, parallel safe).
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "lan-mouse-qa-bidirectional-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
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

/// Forward (into the peer screen) motion of `px` px in source coordinates for
/// a capture whose peer is at `pos`.
fn forward_delta(pos: input_capture::Position, px: f64) -> (f64, f64) {
    match pos {
        input_capture::Position::Left => (-px, 0.0),
        input_capture::Position::Right => (px, 0.0),
        input_capture::Position::Top => (0.0, -px),
        input_capture::Position::Bottom => (0.0, px),
    }
}

/// Reverse (back towards this machine) motion of `px` px in source
/// coordinates for a capture whose peer is at `pos`.
fn reverse_delta(pos: input_capture::Position, px: f64) -> (f64, f64) {
    let (fx, fy) = forward_delta(pos, px);
    (-fx, -fy)
}

// ---------------------------------------------------------------------------
// Static contract pins for the bidirectional fix constants.
// ---------------------------------------------------------------------------

/// The bidirectional fix must stay a small, bounded filter: a wide entry band
/// or a long guard would swallow deliberate hand-backs, and the receiver
/// deadband must stay a few pixels so a real reverse still works.
#[test]
fn the_bidirectional_guard_and_deadband_stay_bounded() {
    let capture = include_str!("../src/capture.rs");
    assert!(
        !capture.contains("ENTER_MOTION_SETTLE"),
        "a timed all-motion settle must never return"
    );
    assert!(
        capture.contains("should_report_enter_only_begin")
            && capture.contains("remote_peer_since")
            && capture.contains("enter_only_take_back_sent"),
        "the EnterOnly re-entry gate must stay wired in the capture task"
    );
    assert!(
        capture.contains("handoff_inward_px") && capture.contains("HANDOFF_ENTRY_BAND_PX"),
        "the source guard must stay position-aware"
    );

    let band: f64 = capture
        .split_once("HANDOFF_ENTRY_BAND_PX: f64 = ")
        .and_then(|(_, rest)| rest.split(';').next())
        .and_then(|value| value.trim().parse().ok())
        .expect("HANDOFF_ENTRY_BAND_PX must stay a named numeric constant");
    assert!(
        band > 0.0 && band <= 32.0,
        "the near-edge band must stay small (got {band} px)"
    );
    let bound_ms: u64 = capture
        .split_once("HANDOFF_POSITION_GUARD: Duration = Duration::from_millis(")
        .and_then(|(_, rest)| rest.split(')').next())
        .and_then(|value| value.trim().parse().ok())
        .expect("HANDOFF_POSITION_GUARD must stay a named millisecond constant");
    assert!(
        bound_ms > 0 && bound_ms <= 400,
        "the position guard bound must stay short (got {bound_ms} ms)"
    );
    let dwell_ms: u64 = capture
        .split_once("REMOTE_ENTRY_DWELL: Duration = Duration::from_millis(")
        .and_then(|(_, rest)| rest.split(')').next())
        .and_then(|value| value.trim().parse().ok())
        .expect("REMOTE_ENTRY_DWELL must stay a named millisecond constant");
    assert!(
        dwell_ms > 0 && dwell_ms <= 1000,
        "the remote-entry dwell must stay short (got {dwell_ms} ms)"
    );

    let emulation = include_str!("../input-emulation/src/libei.rs");
    assert!(
        emulation.contains("return_ready()"),
        "the receiver return path must use the task-10 gate"
    );
    let deadband: f64 = emulation
        .split_once("ENTRY_WORK_DEADBAND_PX: f64 = ")
        .and_then(|(_, rest)| rest.split(';').next())
        .and_then(|value| value.trim().parse().ok())
        .expect("ENTRY_WORK_DEADBAND_PX must stay a named numeric constant");
    assert!(
        deadband > 0.0 && deadband <= 8.0,
        "the receiver work deadband must stay small (got {deadband} px)"
    );
    let settle_ms: u64 = emulation
        .split_once("ENTRY_RETURN_SETTLE: Duration = Duration::from_millis(")
        .and_then(|(_, rest)| rest.split(')').next())
        .and_then(|value| value.trim().parse().ok())
        .expect("ENTRY_RETURN_SETTLE must stay a named millisecond constant");
    assert!(
        settle_ms > 0 && settle_ms <= 500,
        "the receiver entrance settle must stay short (got {settle_ms} ms)"
    );
}

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
