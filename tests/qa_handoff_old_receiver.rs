//! Independent QA harness: Ubuntu (source) -> **old Windows 0.2.18 receiver**
//! (release `5233b05`).
//!
//! The user still runs the old Windows build while the Ubuntu side is the
//! current tree. That receiver parks the remote pointer exactly on the entry
//! edge for an `EnterAt` crossing and asks for the capture back (`LeaveAt`) as
//! soon as the tracked pointer is pushed a few pixels *past* that edge. The
//! final source-side guard (task-7, window-only) suppresses the touchpad's
//! activation backswing for a fixed 200 ms after `Begin`, regardless of how
//! large the reverse burst is, so the old receiver must never see a reverse
//! delta inside that window.
//!
//! The production state machine (`capture::CaptureTask`) lives in a private
//! module of the `lan-mouse` binary crate, so this harness models the two
//! deterministic halves as pure functions (no sockets, no sleeps, no display;
//! virtual microsecond timestamps only):
//!
//! 1. [`OldReceiverModel`] - the release contract model used by the production
//!    unit test (`RETURN_MARGIN_PX = 3.0`, `EnterAt` anchor on the entry edge,
//!    a return fires once the raw outward position passes the margin), plus
//!    [`OldAbsoluteTracker`], a faithful transcription of
//!    `input-emulation/src/libei.rs::AbsoluteTracker::apply` at `5233b05`
//!    (clamp/snap into the virtual screen, near-edge check, `fired` latch with
//!    a 20 px re-arm inset);
//! 2. [`SourceGuard`] - the current-tree guard (`8b6981d`, drops reverse
//!    deltas only until the first forward delta) and the final window-only
//!    guard (`HANDOFF_REVERSE_GUARD = 200 ms`): every reverse delta inside the
//!    fixed window is dropped regardless of size, `HANDOFF_RESIDUAL_MAX_PX`
//!    is trace telemetry only (`remaining_px` saturates at 0 and never ends
//!    the guard), forward deltas stay immediate, and reverse motion passes
//!    again once the window ends.
//!
//! The replay corpus in [`Scene`]s is reconstructed from the real
//! `~/.cache/lan-mouse/handoff-trace.jsonl` records (every magnitude cites a
//! `stage`/`serial`/`dx`/`dy` record in [`TraceBasis`]). The old receiver
//! consumes the *coalesced* delta the source actually sends, so each observed
//! aggregate is replayed as one receiver-visible event. The provenance check
//! [`corpus_is_grounded_in_the_local_handoff_trace`] validates that grounding
//! against the trace files when they exist on the machine (it skips in CI,
//! where the trace is absent).
//!
//! Result: the current-tree guard leaks the post-forward residual into the old
//! receiver (both receiver models fire a `LeaveAt`); the final window-only
//! guard suppresses every reverse delta inside the 200 ms window - including
//! the trace's 34.9 px serial-2 burst and the single 63.27 px serial-3 warp -
//! forwards the first forward delta immediately, and still delivers a
//! deliberate return after the window. See
//! `docs/testing/handoff-old-receiver-harness.md`.
//!
//! Layer: L2 deterministic contract tests. No production file is edited.

#![allow(dead_code)]

use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

/// Production coalescer, compiled read-only into this test crate (same
/// technique as `tests/qa_handoff_shutdown.rs`): the "forward deltas are not
/// delayed" contract is asserted against the real leading-edge/burst logic.
#[path = "../src/coalesce.rs"]
mod coalesce;

use coalesce::MotionCoalescer;

/// The production guard constants, pinned here so the model cannot silently
/// drift from `src/capture.rs` (see
/// `harness_guard_model_matches_the_production_guard`).
///
/// The 32 px value is **trace telemetry only** since task-7: it seeds
/// `remaining_px`, saturates at 0 and never ends the guard. The fixed
/// 200 ms window is the only suppression decision.
const HANDOFF_RESIDUAL_MAX_PX: f64 = 32.0;
const HANDOFF_REVERSE_GUARD: Duration = Duration::from_millis(200);

/// Source of the production guard, compiled into the test binary for the
/// model/implementation parity check.
const CAPTURE_SOURCE: &str = include_str!("../src/capture.rs");

/// `input_capture::CaptureHandle` used by the coalescer contract test.
const HANDLE: input_capture::CaptureHandle = 7;

// ===========================================================================
// 1. Old receiver model (release `5233b05`)
// ===========================================================================

/// Extra pixels the pointer must be pushed past the entry edge before the old
/// receiver hands control back. Verbatim from
/// `input-emulation/src/libei.rs` at release `5233b05`
/// (`const RETURN_MARGIN_PX: f64 = 3.0;`).
const RETURN_MARGIN_PX: f64 = 3.0;

/// Distance the old receiver parks the pointer inside the entry edge when the
/// peer does **not** send `EnterAt` (`EDGE_ANCHOR_INSET` at `5233b05`). The
/// `EnterAt` path, which the current source uses, anchors on the entry edge
/// itself, which is the model below.
const EDGE_ANCHOR_INSET_PX: f64 = 24.0;

/// How far inside the entry region the pointer must move again before the old
/// tracker can fire a second return (`AbsoluteTracker::apply` at `5233b05`).
const REARM_INSET_PX: f64 = 20.0;

/// Typical receiver screen: a single 1920x1080 monitor. The circled entry edge
/// is horizontal, so only the x axis is exercised; y only carries the `EnterAt`
/// coordinate along the edge.
const SCREEN_W: f64 = 1920.0;
const SCREEN_H: f64 = 1080.0;

/// Crossing coordinate (along the shared edge) sent by `EnterAt`; `0.5` keeps
/// the anchor in the middle of the edge just like the trace's `along` samples.
const ENTER_AT_ALONG: f64 = 0.5;

/// Which screen edge the remote pointer entered through on the receiver.
///
/// The observed trace is always a source controller with `pos: Some(Right)`
/// (the peer screen is to the right of the Ubuntu screen), so the old receiver
/// is entered through its **left** edge. Both horizontal edges are modelled
/// because the old tracker is asymmetric; only the left one is exercised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryEdge {
    Left,
    Right,
}

impl EntryEdge {
    /// Sign of an x delta that moves the tracked pointer *out of* the screen
    /// through this edge (towards the shared edge).
    fn outward_sign(self) -> f64 {
        match self {
            EntryEdge::Left => -1.0,
            EntryEdge::Right => 1.0,
        }
    }
}

/// Which screen the pointer crosses to on the source side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Crossing {
    /// Peer screen to the right of the source screen: forward deltas are +dx,
    /// and the receiver is entered through its left edge.
    PeerRight,
    /// Peer screen to the left of the source screen: forward deltas are -dx,
    /// and the receiver is entered through its right edge.
    PeerLeft,
}

impl Crossing {
    /// Signed outward component of a pointer motion on the source side.
    ///
    /// This is the harness counterpart of `handoff_outward_px` in
    /// `src/capture.rs`: positive means the motion points back out of the
    /// screen the pointer crosses into, i.e. the activation warp/backswing.
    fn outward_px(self, dx: f64, _dy: f64) -> f64 {
        match self {
            Crossing::PeerRight => -dx,
            Crossing::PeerLeft => dx,
        }
    }

    /// Source-side classification of one delta.
    fn motion_kind(self, dx: f64) -> MotionKind {
        let outward = self.outward_px(dx, 0.0);
        if outward > 0.0 {
            MotionKind::Reverse
        } else if outward < 0.0 {
            MotionKind::Forward
        } else {
            MotionKind::Neutral
        }
    }

    fn receiver_edge(self) -> EntryEdge {
        match self {
            Crossing::PeerRight => EntryEdge::Left,
            Crossing::PeerLeft => EntryEdge::Right,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MotionKind {
    /// Carries the remote pointer into its screen.
    Forward,
    /// Activation warp/backswing: points out of the receiver's entry edge.
    Reverse,
    /// Perpendicular to the crossing axis (no outward component).
    Neutral,
}

/// Behavioural model of the old receiver's return detector.
///
/// This is the contract model also used by the production unit test: it tracks
/// the *raw* outward position on the hand-off axis (the entry anchor is 0, the
/// position is not clamped into the screen) and reports a return once it is
/// pushed more than [`RETURN_MARGIN_PX`] past the entry edge. A model that is
/// this strict still fires for every reverse residual that the current tree
/// forwards, and does not fire for any delta the final window-only guard drops.
#[derive(Clone, Copy, Debug)]
struct OldReceiverModel {
    edge: EntryEdge,
    /// Raw outward position relative to the `EnterAt` anchor, in logical px.
    /// Negative = inside the receiver's screen.
    outward_px: f64,
    /// Number of `LeaveAt` requests the old receiver emitted (it latches after
    /// the first one).
    returns: u32,
}

impl OldReceiverModel {
    /// Arm exactly like release `5233b05` does for an `EnterAt` hand-off: park
    /// the pointer on the entry edge itself, at the crossing coordinate.
    fn armed_at_entry_edge(edge: EntryEdge) -> Self {
        Self {
            edge,
            outward_px: 0.0,
            returns: 0,
        }
    }

    /// Apply one forwarded delta and report the entry edge once the raw
    /// outward position passes [`RETURN_MARGIN_PX`].
    fn apply(&mut self, dx: f64, dy: f64) -> Option<EntryEdge> {
        if !dx.is_finite() || !dy.is_finite() {
            return None;
        }
        self.outward_px += self.edge.outward_sign() * dx;
        if self.returns == 0 && self.outward_px > RETURN_MARGIN_PX {
            self.returns += 1;
            return Some(self.edge);
        }
        None
    }
}

/// Faithful transcription of `AbsoluteTracker::apply` from
/// `input-emulation/src/libei.rs` at release `5233b05` for a single-monitor
/// layout, `delta_scale = 1.0` and the `EnterAt` anchor on the entry edge.
///
/// This is the real absolute tracker rather than the raw-position contract
/// model: it clamps and snaps the injected position into the entry region (as
/// the compositor does), requires the *snapped* position to still be at the
/// entry edge, and latches `fired` until the pointer is 20 px inside the
/// screen again. It is used as a second, independent oracle: both models must
/// agree on every replay.
#[derive(Clone, Copy, Debug)]
struct OldAbsoluteTracker {
    edge: EntryEdge,
    width: f64,
    height: f64,
    x: f64,
    y: f64,
    /// Set once the hand-off has been requested; re-armed 20 px inside.
    fired: bool,
    returns: u32,
}

impl OldAbsoluteTracker {
    /// Arm on the entry edge at the `EnterAt` crossing coordinate.
    fn armed_at_entry_edge(edge: EntryEdge, width: f64, height: f64, along: f64) -> Self {
        let along = along.clamp(0.0, 1.0);
        let y = along * (height - 1.0);
        let x = match edge {
            // `entry_point_at`: Left anchors on the region's first pixel,
            // Right on the region's last pixel inside the screen.
            EntryEdge::Left => 0.0,
            EntryEdge::Right => width - 1.0,
        };
        Self {
            edge,
            width,
            height,
            x,
            y,
            fired: false,
            returns: 0,
        }
    }

    fn position(&self) -> (f64, f64) {
        (self.x, self.y)
    }

    /// One `AbsoluteTracker::apply` step (`5233b05`), minus the sensitivity
    /// scaling and the corrupted-delta guard, which do not influence the
    /// hand-off axis decisions exercised here.
    fn apply(&mut self, dx: f64, dy: f64) -> Option<EntryEdge> {
        if !dx.is_finite() || !dy.is_finite() {
            return None;
        }
        let raw_x = self.x + dx;
        let raw_y = self.y + dy;
        // The compositor clamps the real cursor into the layout; mirror it.
        let (clamped_x, clamped_y) = (
            raw_x.clamp(0.0, self.width - 1.0),
            raw_y.clamp(0.0, self.height - 1.0),
        );
        // Single-monitor `snap`: the clamped position already sits in the one
        // region, so tracking equals the sent position.
        let (x, y) = (clamped_x, clamped_y);

        if self.fired {
            self.x = x;
            self.y = y;
            let inset = REARM_INSET_PX;
            if self.x > inset
                && self.x < self.width - 1.0 - inset
                && self.y > inset
                && self.y < self.height - 1.0 - inset
            {
                self.fired = false;
            }
            return None;
        }

        // The half-open entry-region bounds `(x0, y0, x1, y1)` of the monitor.
        let (x0, x1) = (0.0, self.width);

        let near_edge = match self.edge {
            EntryEdge::Left => x <= x0 + 1.0,
            EntryEdge::Right => x >= x1 - 2.0,
        };
        let past_edge = match self.edge {
            EntryEdge::Left => raw_x < x0 - RETURN_MARGIN_PX,
            EntryEdge::Right => raw_x > x1 + RETURN_MARGIN_PX,
        };

        self.x = x;
        self.y = y;
        if near_edge && past_edge {
            self.fired = true;
            self.returns += 1;
            // Snap onto the edge so reversing the push moves again at once.
            match self.edge {
                EntryEdge::Left => self.x = x0,
                EntryEdge::Right => self.x = x1 - 1.0,
            }
            return Some(self.edge);
        }
        None
    }
}

// ===========================================================================
// 2. Source-side reverse-motion guards under test
// ===========================================================================

/// One pointer motion captured on the source, at a virtual microsecond
/// timestamp relative to `Begin` (the hand-off start).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Motion {
    at_us: u64,
    dx: f64,
    dy: f64,
}

const fn motion(at_us: u64, dx: f64, dy: f64) -> Motion {
    Motion { at_us, dx, dy }
}

/// What the source-side guard does with one captured motion.
#[derive(Clone, Copy, Debug, PartialEq)]
enum GuardDecision {
    /// Forward the delta to the peer immediately.
    Forward,
    /// Reverse delta swallowed by the hand-off guard; the peer never sees it.
    DropReverse {
        /// Outward distance of the dropped delta, in logical px.
        outward_px: f64,
        /// The production trace's `remaining_px` telemetry: it starts at
        /// [`HANDOFF_RESIDUAL_MAX_PX`], decreases with every dropped delta and
        /// **saturates at 0**; it never ends the guard.
        remaining_px: f64,
        /// Microseconds since `Begin`; mirrors the production trace field.
        elapsed_us: u64,
        /// Microseconds left in the fixed guard window; production trace field.
        guard_remaining_us: u64,
    },
}

impl GuardDecision {
    fn is_drop(self) -> bool {
        matches!(self, GuardDecision::DropReverse { .. })
    }
}

/// The two source-side guard implementations being compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceGuardKind {
    /// `src/capture.rs` @ `8b6981d` (the tree the user reports the bug on):
    /// the residual filter is disarmed by the **first forward delta**, so a
    /// backswing residual that arrives *after* the crossing motion reaches the
    /// old receiver.
    CurrentTree,
    /// `src/capture.rs` after task-7 (final, frozen window-only guard):
    /// every reverse delta inside the fixed [`HANDOFF_REVERSE_GUARD`] window
    /// after `Begin` is dropped **regardless of size**; forward deltas stay
    /// immediate; reverse motion is forwarded again once the window ends.
    /// [`HANDOFF_RESIDUAL_MAX_PX`] is trace telemetry only.
    WindowOnly,
}

/// Pure model of the source-side hand-off guard.
///
/// [`SourceGuardKind::WindowOnly`] is transcribed line-for-line from the final
/// production guard in `src/capture.rs` (task-7, frozen at
/// `src/capture.rs` sha256 `a1a3909f...`):
///
/// ```text
/// if let Some(remaining_px) = self.residual_reverse_px {
///     match self.reverse_guard_until.filter(|until| now < *until) {
///         None => clear guard and forward,
///         Some(until) => {
///             let reverse_px = handoff_outward_px(pos, dx, dy).max(0.0);
///             if reverse_px > 0.0 {
///                 // drop the WHOLE delta, however large; telemetry saturates:
///                 let left_px = (remaining_px - reverse_px).max(0.0);
///                 // ... trace only; the guard stays armed
///                 drop delta;
///             }
///         }
///     }
/// }
/// ```
///
/// The 32 px constant no longer appears in any decision: the window is the
/// only condition, so a `-17.96 + -16.94 px` burst (34.9 px) or a single
/// `-63.27 px` warp inside the window is dropped whole.
#[derive(Clone, Copy, Debug)]
struct SourceGuard {
    kind: SourceGuardKind,
    crossing: Crossing,
    began_at_us: u64,
    /// Remaining suppressed reverse distance (`residual_reverse_px`): trace
    /// telemetry only, seeded with [`HANDOFF_RESIDUAL_MAX_PX`], saturating at
    /// 0. It does not decide anything.
    remaining_px: Option<f64>,
    /// Deadline of the fixed window (`reverse_guard_until`).
    guard_until_us: Option<u64>,
}

impl SourceGuard {
    /// `Begin`/`BeginAt`: arm the guard for this hand-off.
    fn after_begin(kind: SourceGuardKind, crossing: Crossing) -> Self {
        let began_at_us = 0;
        Self {
            kind,
            crossing,
            began_at_us,
            remaining_px: Some(HANDOFF_RESIDUAL_MAX_PX),
            guard_until_us: match kind {
                SourceGuardKind::CurrentTree => None,
                SourceGuardKind::WindowOnly => {
                    Some(began_at_us + HANDOFF_REVERSE_GUARD.as_micros() as u64)
                }
            },
        }
    }

    fn on_motion(&mut self, motion: Motion) -> GuardDecision {
        match self.kind {
            SourceGuardKind::CurrentTree => self.current_tree(motion),
            SourceGuardKind::WindowOnly => self.window_only(motion),
        }
    }

    /// `src/capture.rs` @ `8b6981d`, transcribed verbatim: the leading reverse
    /// residual is dropped until the first forward delta, bounded by the same
    /// 32 px budget. A reverse delta past the budget is forwarded whole.
    fn current_tree(&mut self, motion: Motion) -> GuardDecision {
        let Some(remaining_px) = self.remaining_px else {
            return GuardDecision::Forward;
        };
        let reverse_px = self.crossing.outward_px(motion.dx, motion.dy).max(0.0);
        if reverse_px <= 0.0 {
            // The crossing motion itself: stop filtering so a later return
            // push is forwarded normally. (This is the behaviour the final
            // window-only guard fixes.)
            self.remaining_px = None;
            GuardDecision::Forward
        } else if reverse_px < remaining_px {
            let left_px = remaining_px - reverse_px;
            self.remaining_px = Some(left_px);
            GuardDecision::DropReverse {
                outward_px: reverse_px,
                remaining_px: left_px,
                elapsed_us: motion.at_us.saturating_sub(self.began_at_us),
                guard_remaining_us: 0,
            }
        } else {
            // More reverse motion than the activation residual can contain:
            // a deliberate hand-back, forwarded whole.
            self.remaining_px = None;
            GuardDecision::Forward
        }
    }

    /// Final window-only guard (task-7), transcribed from the landed
    /// production fix: the fixed 200 ms window is the only decision.
    fn window_only(&mut self, motion: Motion) -> GuardDecision {
        let Some(remaining_px) = self.remaining_px else {
            return GuardDecision::Forward;
        };
        // Lazy window expiry: checked on the next pointer event, exactly like
        // `reverse_guard_until.filter(|until| now < *until)`.
        let Some(until) = self.guard_until_us.filter(|until| motion.at_us < *until) else {
            self.remaining_px = None;
            self.guard_until_us = None;
            return GuardDecision::Forward;
        };

        let reverse_px = self.crossing.outward_px(motion.dx, motion.dy).max(0.0);
        if reverse_px <= 0.0 {
            // Forward/neutral motion is never delayed and never disarms the
            // guard: the old receiver can still return on the backswing that
            // arrives after the crossing motion.
            return GuardDecision::Forward;
        }
        // Drop the whole delta, however large. The remaining distance is trace
        // telemetry only and saturates at 0; it never ends the guard.
        let left_px = (remaining_px - reverse_px).max(0.0);
        self.remaining_px = Some(left_px);
        GuardDecision::DropReverse {
            outward_px: reverse_px,
            remaining_px: left_px,
            elapsed_us: motion.at_us.saturating_sub(self.began_at_us),
            guard_remaining_us: until.saturating_sub(motion.at_us),
        }
    }

    fn guard_until_us(&self) -> Option<u64> {
        self.guard_until_us
    }

    fn remaining_px(&self) -> Option<f64> {
        self.remaining_px
    }
}

// ===========================================================================
// 3. Replay corpus expanded from the observed hand-off trace
// ===========================================================================

/// One observed `capture.*` record the corpus magnitudes are taken from.
///
/// The trace logs the *coalesced* motion the source sent and the old receiver
/// consumes exactly those coalesced packets, so each observed aggregate is
/// replayed as one receiver-visible event; only the sequence placement is
/// reconstructed.
#[derive(Clone, Copy, Debug)]
struct TraceBasis {
    stage: &'static str,
    serial: u32,
    dx: f64,
    dy: f64,
}

/// How the source guard is supposed to treat the scene.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SceneExpectation {
    /// Current tree leaks a post-forward reverse residual into the old
    /// receiver; the final window-only guard must suppress every reverse delta
    /// inside the fixed 200 ms window, regardless of size.
    GuardMustSuppress,
    /// Deliberate hand-back that starts after the 200 ms window: both guards
    /// must forward it (the window must not swallow genuine returns).
    DeliberateReturnAfterWindow,
}

/// A deterministic delta sequence on the source between `Begin` and the end of
/// the hand-off replay.
#[derive(Clone, Copy, Debug)]
struct Scene {
    name: &'static str,
    /// Why the sequence is realistic (trace evidence).
    why: &'static str,
    crossing: Crossing,
    /// `EnterAt` coordinate along the shared edge (0.0..=1.0).
    along: f64,
    /// Motions in capture order, `at_us` relative to `Begin`.
    events: &'static [Motion],
    /// Trace records backing the magnitudes in `events`.
    basis: &'static [TraceBasis],
    expectation: SceneExpectation,
}

/// Slow crossing: the first forward delta is tiny (the user barely moved) and
/// the compositor warp arrives right after it. Current tree forwards the warp
/// residual (-4.02 px) into the old receiver, whose outward position becomes
/// `-0.458 + 4.020 = 3.562 px > 3 px` -> `LeaveAt`.
static SCENE_SLOW_EVENTS: &[Motion] = &[
    // Touchpad backswing while the hand-off arms.
    motion(120, -1.4109896421432495, -1.4109896421432495),
    // The crossing motion itself (tiny, slow touchpad move).
    motion(300, 0.4580078125, 1.3053388595581055),
    // Compositor warp: arrives after the crossing motion.
    motion(5_000, -4.020392417907715, 2.1487011909484863),
    // Settling forward motion towards the peer screen.
    motion(9_000, 0.8673844337463379, 0.0),
];

/// Medium crossing: first forward 3.02 px, then a full 9.07 px backswing
/// residual. Current tree forwards it: `-3.025 + 9.074 = 6.049 px` -> return.
static SCENE_MEDIUM_EVENTS: &[Motion] = &[
    motion(90, -3.8841233253479004, 4.8841233253479),
    motion(250, 3.0247998237609863, 0.0),
    motion(6_000, -9.0743989944458, 9.0743989944458),
    motion(10_000, 1.5123999118804932, 0.0),
];

/// The aggregate warp observed in the active trace (`serial 2`,
/// `-17.960790634155273`) replayed as the one coalesced reverse delta the old
/// receiver would have seen, followed by a small forward, then a post-forward
/// residual. Current tree drops the leading aggregate but forwards the
/// residual: `-3.025 + 9.074 = 6.049 px` -> return.
static SCENE_TRACE_BURST_EVENTS: &[Motion] = &[
    motion(100, -17.960790634155273, 1.6327991485595703),
    motion(400, 3.0247998237609863, 0.0),
    motion(7_000, -9.0743989944458, 9.0743989944458),
];

/// Deliberate hand-back: the reverse push starts only after the 200 ms guard
/// window, so it must reach the old receiver and still return.
static SCENE_GENUINE_RETURN_EVENTS: &[Motion] = &[
    motion(1_000, 3.0247998237609863, 0.0),
    motion(250_000, -9.0743989944458, 9.0743989944458),
];

/// The full aggregate pair observed in the active trace (`serial 2`):
/// activation burst -17.96 px, first forward +12.39 px, then the next
/// coalesced warp -16.94 px. The reverse sum (`34.9 px`) exceeds the old 32 px
/// accumulation bound - the hole task-7 closed: under the final window-only
/// guard both reverse deltas are dropped whole inside the 200 ms window.
static SCENE_SERIAL2_BURST_EVENTS: &[Motion] = &[
    motion(100, -17.960790634155273, 1.6327991485595703),
    motion(400, 12.39034652709961, -1.1390466690063477),
    motion(1_000, -16.937875747680664, 4.619420528411865),
];

/// The active trace's `serial 3` single reverse warp: -63.27 px in one
/// coalesced first motion (the largest observed burst), then the `serial 4`
/// forward correction +65.34 px. The final guard drops the warp whole inside
/// the window; the old receiver only sees the forward motion.
static SCENE_SERIAL3_WARP_EVENTS: &[Motion] = &[
    motion(127, -63.27177047729492, 13.754733085632324),
    motion(48_000, 65.33959197998047, -11.189517974853516),
];

const SCENE_BASIS_SLOW: &[TraceBasis] = &[
    TraceBasis {
        stage: "capture.reverse_motion_dropped",
        serial: 7,
        dx: -1.4109896421432495,
        dy: -1.4109896421432495,
    },
    TraceBasis {
        stage: "capture.first_motion_sent",
        serial: 19,
        dx: 0.4580078125,
        dy: 1.3053388595581055,
    },
    TraceBasis {
        stage: "capture.first_motion_sent",
        serial: 20,
        dx: -4.020392417907715,
        dy: 2.1487011909484863,
    },
    TraceBasis {
        stage: "capture.first_motion_sent",
        serial: 8,
        dx: 0.8673844337463379,
        dy: 0.0,
    },
];

const SCENE_BASIS_MEDIUM: &[TraceBasis] = &[
    TraceBasis {
        stage: "capture.first_motion_sent",
        serial: 25,
        dx: -3.8841233253479004,
        dy: 4.8841233253479,
    },
    TraceBasis {
        stage: "capture.first_motion_sent",
        serial: 2,
        dx: 3.0247998237609863,
        dy: 0.0,
    },
    TraceBasis {
        stage: "capture.first_motion_sent",
        serial: 4,
        dx: -9.0743989944458,
        dy: 9.0743989944458,
    },
    TraceBasis {
        stage: "capture.first_motion_sent",
        serial: 9,
        dx: 1.5123999118804932,
        dy: 0.0,
    },
];

const SCENE_BASIS_TRACE_BURST: &[TraceBasis] = &[
    TraceBasis {
        stage: "capture.reverse_motion_dropped",
        serial: 2,
        dx: -17.960790634155273,
        dy: 1.6327991485595703,
    },
    TraceBasis {
        stage: "capture.first_motion_sent",
        serial: 2,
        dx: 3.0247998237609863,
        dy: 0.0,
    },
    TraceBasis {
        stage: "capture.first_motion_sent",
        serial: 4,
        dx: -9.0743989944458,
        dy: 9.0743989944458,
    },
];

const SCENE_BASIS_GENUINE_RETURN: &[TraceBasis] = &[
    TraceBasis {
        stage: "capture.first_motion_sent",
        serial: 2,
        dx: 3.0247998237609863,
        dy: 0.0,
    },
    TraceBasis {
        stage: "capture.first_motion_sent",
        serial: 4,
        dx: -9.0743989944458,
        dy: 9.0743989944458,
    },
];

const SCENE_BASIS_SERIAL2_BURST: &[TraceBasis] = &[
    TraceBasis {
        stage: "capture.reverse_motion_dropped",
        serial: 2,
        dx: -17.960790634155273,
        dy: 1.6327991485595703,
    },
    TraceBasis {
        stage: "capture.first_motion_sent",
        serial: 2,
        dx: 12.39034652709961,
        dy: -1.1390466690063477,
    },
    TraceBasis {
        stage: "capture.reverse_motion_dropped",
        serial: 2,
        dx: -16.937875747680664,
        dy: 4.619420528411865,
    },
];

const SCENE_BASIS_SERIAL3_WARP: &[TraceBasis] = &[
    TraceBasis {
        stage: "capture.first_motion_sent",
        serial: 3,
        dx: -63.27177047729492,
        dy: 13.754733085632324,
    },
    TraceBasis {
        stage: "capture.first_motion_sent",
        serial: 4,
        dx: 65.33959197998047,
        dy: -11.189517974853516,
    },
];

const SCENE_SLOW: Scene = Scene {
    name: "slow_crossing_then_warp_residual",
    why: "tiny first forward (0.46 px) followed by a -4.02 px compositor warp, \
          both observed as captured deltas",
    crossing: Crossing::PeerRight,
    along: 0.5,
    events: SCENE_SLOW_EVENTS,
    basis: SCENE_BASIS_SLOW,
    expectation: SceneExpectation::GuardMustSuppress,
};

const SCENE_MEDIUM: Scene = Scene {
    name: "medium_crossing_then_full_backswing",
    why: "first forward 3.02 px then a full -9.07 px backswing residual",
    crossing: Crossing::PeerRight,
    along: 0.42,
    events: SCENE_MEDIUM_EVENTS,
    basis: SCENE_BASIS_MEDIUM,
    expectation: SceneExpectation::GuardMustSuppress,
};

const SCENE_TRACE_BURST: Scene = Scene {
    name: "trace_serial2_burst_then_residual",
    why: "the active trace's -17.96 px activation aggregate, a small forward, \
          then a -9.07 px post-forward residual",
    crossing: Crossing::PeerRight,
    along: 0.56,
    events: SCENE_TRACE_BURST_EVENTS,
    basis: SCENE_BASIS_TRACE_BURST,
    expectation: SceneExpectation::GuardMustSuppress,
};

const SCENE_GENUINE_RETURN: Scene = Scene {
    name: "deliberate_return_after_the_guard",
    why: "normal crossing, then a deliberate -9.07 px hand-back at 250 ms \
          (after the 200 ms guard window)",
    crossing: Crossing::PeerRight,
    along: 0.5,
    events: SCENE_GENUINE_RETURN_EVENTS,
    basis: SCENE_BASIS_GENUINE_RETURN,
    expectation: SceneExpectation::DeliberateReturnAfterWindow,
};

const SCENE_SERIAL2_BURST: Scene = Scene {
    name: "serial2_34_9px_burst",
    why: "the active trace's full -17.96/+12.39/-16.94 px serial 2 pattern: the \
          reverse sum (34.9 px) exceeds the old 32 px accumulation bound and \
          must still be dropped whole inside the window",
    crossing: Crossing::PeerRight,
    along: 0.56,
    events: SCENE_SERIAL2_BURST_EVENTS,
    basis: SCENE_BASIS_SERIAL2_BURST,
    expectation: SceneExpectation::GuardMustSuppress,
};

const SCENE_SERIAL3_WARP: Scene = Scene {
    name: "serial3_63_27px_single_warp",
    why: "the active trace's single -63.27 px serial 3 reverse warp, then the \
          +65.34 px serial 4 forward correction",
    crossing: Crossing::PeerRight,
    along: 0.59,
    events: SCENE_SERIAL3_WARP_EVENTS,
    basis: SCENE_BASIS_SERIAL3_WARP,
    expectation: SceneExpectation::GuardMustSuppress,
};

/// Scenes the guard is expected to fully suppress.
const SCENES_GUARD_MUST_SUPPRESS: &[Scene] = &[
    SCENE_SLOW,
    SCENE_MEDIUM,
    SCENE_TRACE_BURST,
    SCENE_SERIAL2_BURST,
    SCENE_SERIAL3_WARP,
];

/// Every scene of the corpus (used by the trace-provenance check).
const ALL_SCENES: &[Scene] = &[
    SCENE_SLOW,
    SCENE_MEDIUM,
    SCENE_TRACE_BURST,
    SCENE_GENUINE_RETURN,
    SCENE_SERIAL2_BURST,
    SCENE_SERIAL3_WARP,
];

// ===========================================================================
// 4. Replay engine
// ===========================================================================

/// What one scene replay observed.
#[derive(Debug, Default)]
struct ReplayReport {
    /// Guard decision per captured motion, in capture order.
    decisions: Vec<GuardDecision>,
    /// Deltas that actually reached the old receiver, in order.
    forwarded: Vec<Motion>,
    /// Reverse deltas that reached the old receiver (should be empty inside
    /// the guard).
    forwarded_reverse: Vec<Motion>,
    dropped_events: usize,
    dropped_reverse_px: f64,
    forward_events: usize,
    forward_events_forwarded: usize,
    /// Timestamp of the first *forward* delta that was forwarded.
    first_forward_forwarded_at_us: Option<u64>,
    /// Old raw-position model: returns and cumulative outward position.
    old_raw_returns: u32,
    old_raw_outward_px: f64,
    old_raw_first_return_at_us: Option<u64>,
    /// Raw outward position at the moment of the first return (the replay may
    /// continue with forward motion afterwards, pulling the final position back
    /// inside the margin).
    old_raw_outward_at_return: Option<f64>,
    /// Faithful `AbsoluteTracker` model: returns and final position.
    old_tracker_returns: u32,
    old_tracker_first_return_at_us: Option<u64>,
    old_tracker_position: (f64, f64),
    /// Inward displacement of the raw model right after the first forwarded
    /// forward delta (positive = the remote cursor moved into its screen).
    inward_after_first_forward_px: Option<f64>,
}

/// Replay one scene through a source guard and both old-receiver models.
fn replay(kind: SourceGuardKind, scene: &Scene) -> ReplayReport {
    replay_events(kind, scene.crossing, scene.along, scene.events)
}

/// Replay an explicit delta sequence (used by the scene corpus and by the
/// deterministic sweep, which builds sequences on the fly).
fn replay_events(
    kind: SourceGuardKind,
    crossing: Crossing,
    along: f64,
    events: &[Motion],
) -> ReplayReport {
    let mut guard = SourceGuard::after_begin(kind, crossing);
    let mut raw = OldReceiverModel::armed_at_entry_edge(crossing.receiver_edge());
    let mut tracker = OldAbsoluteTracker::armed_at_entry_edge(
        crossing.receiver_edge(),
        SCREEN_W,
        SCREEN_H,
        along,
    );
    let mut report = ReplayReport::default();

    for captured in events {
        let decision = guard.on_motion(*captured);
        report.decisions.push(decision);

        let motion_kind = crossing.motion_kind(captured.dx);
        if motion_kind == MotionKind::Forward {
            report.forward_events += 1;
        }

        if decision.is_drop() {
            if let GuardDecision::DropReverse { outward_px, .. } = decision {
                report.dropped_events += 1;
                report.dropped_reverse_px += outward_px;
            }
            continue;
        }

        report.forwarded.push(*captured);
        if motion_kind == MotionKind::Forward {
            report.forward_events_forwarded += 1;
            if report.first_forward_forwarded_at_us.is_none() {
                report.first_forward_forwarded_at_us = Some(captured.at_us);
            }
        } else if motion_kind == MotionKind::Reverse {
            report.forwarded_reverse.push(*captured);
        }

        if raw.apply(captured.dx, captured.dy).is_some()
            && report.old_raw_first_return_at_us.is_none()
        {
            report.old_raw_first_return_at_us = Some(captured.at_us);
            report.old_raw_outward_at_return = Some(raw.outward_px);
        }
        if tracker.apply(captured.dx, captured.dy).is_some()
            && report.old_tracker_first_return_at_us.is_none()
        {
            report.old_tracker_first_return_at_us = Some(captured.at_us);
        }

        if motion_kind == MotionKind::Forward && report.inward_after_first_forward_px.is_none() {
            report.inward_after_first_forward_px = Some(-raw.outward_px);
        }
    }

    report.old_raw_returns = raw.returns;
    report.old_raw_outward_px = raw.outward_px;
    report.old_tracker_returns = tracker.returns;
    report.old_tracker_position = tracker.position();
    report
}

/// Assert two `f64`s are equal within a micro-pixel tolerance.
fn assert_close(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() <= 1e-6,
        "{what}: expected {expected}, got {actual}"
    );
}

// ===========================================================================
// 5. Tests: old receiver model
// ===========================================================================

#[test]
fn old_receiver_contract_model_fires_past_the_three_pixel_margin() {
    // `EnterAt` anchors exactly on the entry edge and the margin is 3.0 px.
    let mut receiver = OldReceiverModel::armed_at_entry_edge(EntryEdge::Left);
    assert_eq!(
        receiver.apply(-2.999_999, 0.0),
        None,
        "-2.999999 px is still inside the 3 px margin"
    );
    assert_eq!(
        receiver.apply(-0.000_002, 0.0),
        Some(EntryEdge::Left),
        "more than 3 px of raw outward motion must hand control back"
    );
    assert_eq!(
        receiver.returns, 1,
        "the old receiver latches after a return"
    );

    // A larger single reverse delta fires immediately from the edge anchor.
    let mut receiver = OldReceiverModel::armed_at_entry_edge(EntryEdge::Left);
    assert_eq!(
        receiver.apply(-4.020392417907715, 2.1487011909484863),
        Some(EntryEdge::Left)
    );
}

#[test]
fn old_absolute_tracker_agrees_with_the_contract_model() {
    // The faithful tracker anchors exactly on the edge for `EnterAt`.
    let mut tracker = OldAbsoluteTracker::armed_at_entry_edge(
        EntryEdge::Left,
        SCREEN_W,
        SCREEN_H,
        ENTER_AT_ALONG,
    );
    let (x, y) = tracker.position();
    assert_close(x, 0.0, "EnterAt anchor on the left edge");
    assert_close(y, ENTER_AT_ALONG * (SCREEN_H - 1.0), "EnterAt along");

    // A forward delta moves the remote cursor into its screen without a return.
    assert_eq!(tracker.apply(12.39034652709961, 0.0), None);
    assert_close(tracker.position().0, 12.39034652709961, "forward position");

    // A single post-forward residual past the margin still returns.
    assert_eq!(
        tracker.apply(-16.937875747680664, 0.0),
        Some(EntryEdge::Left)
    );
    assert_eq!(tracker.returns, 1);
}

// ===========================================================================
// 6. Tests: current tree FAILS the old-receiver contract
// ===========================================================================

/// Characterises the bug the user reports: the current tree disarms the
/// residual filter at the first forward delta (and forwards a leading burst
/// past the old 32 px bound whole), so reverse motion that arrives afterwards
/// reaches the old receiver and hands control back (the observed `LeaveAt` /
/// enter-leave churn).
#[test]
fn current_tree_source_guard_leaks_the_post_forward_residual() {
    for scene in SCENES_GUARD_MUST_SUPPRESS {
        let report = replay(SourceGuardKind::CurrentTree, scene);
        let label = scene.name;

        assert!(
            !report.forwarded_reverse.is_empty(),
            "{label}: reverse motion reaches the old receiver under the current tree"
        );
        assert_eq!(
            report.old_raw_returns, 1,
            "{label}: the old raw-position receiver must hand control back"
        );
        assert_eq!(
            report.old_tracker_returns, 1,
            "{label}: the faithful AbsoluteTracker must hand control back too"
        );
        assert_eq!(
            report.old_raw_first_return_at_us, report.old_tracker_first_return_at_us,
            "{label}: both receiver models must fire on the same forwarded delta"
        );
        let outward_at_return = report
            .old_raw_outward_at_return
            .expect("the old receiver returned");
        assert!(
            outward_at_return > RETURN_MARGIN_PX,
            "{label}: leaked reverse motion must push the receiver past its margin: {outward_at_return}"
        );
        eprintln!(
            "[current-tree] {label}: forwarded reverse {:?}, raw outward at return = {:.3} px, returned at {:?} us",
            report.forwarded_reverse, outward_at_return, report.old_raw_first_return_at_us
        );
    }
}

// ===========================================================================
// 7. Tests: final window-only guard PASSES the old-receiver contract
// ===========================================================================

/// The final fix must keep every reverse delta of the same scenes away from the
/// old receiver - including the serial-2 34.9 px burst and the serial-3 single
/// 63.27 px warp that the superseded budget guard forwarded - with no `LeaveAt`
/// churn under both receiver models.
#[test]
fn window_only_source_guard_keeps_the_old_receiver_from_returning() {
    for scene in SCENES_GUARD_MUST_SUPPRESS {
        let report = replay(SourceGuardKind::WindowOnly, scene);
        let label = scene.name;

        assert_eq!(
            report.old_raw_returns, 0,
            "{label}: old raw-position receiver must not hand control back"
        );
        assert_eq!(
            report.old_tracker_returns, 0,
            "{label}: faithful AbsoluteTracker must not hand control back"
        );
        assert!(
            report.forwarded_reverse.is_empty(),
            "{label}: no reverse delta may reach the old receiver inside the guard: {:?}",
            report.forwarded_reverse
        );
        assert_eq!(
            report.forward_events, report.forward_events_forwarded,
            "{label}: every forward delta must be forwarded"
        );
        let scene_reverse_px: f64 = scene
            .events
            .iter()
            .map(|m| scene.crossing.outward_px(m.dx, m.dy).max(0.0))
            .sum();
        assert_close(
            report.dropped_reverse_px,
            scene_reverse_px,
            &format!("{label}: every reverse delta must be dropped (window-only)"),
        );
        assert_eq!(
            report.decisions.len(),
            scene.events.len(),
            "{label}: every captured motion must get a decision"
        );
        eprintln!(
            "[window-only] {label}: dropped {:.3} px of reverse motion (telemetry {:?}), forwarded {:?}, receiver outward = {:.3} px",
            report.dropped_reverse_px,
            report.decisions,
            report.forwarded.iter().map(|m| m.dx).collect::<Vec<_>>(),
            report.old_raw_outward_px
        );
    }
}

/// Independent re-derivation of the sequence used by the production unit test
/// `the_reverse_guard_keeps_an_old_receiver_from_handing_control_back` (still
/// valid under the final window-only guard):
/// `+5, +7, -2, +4, +5` raw deltas for a peer at `Position::Left` (forward is
/// `-dx`, reverse is `+dx`). Only the forward crossing motion may reach the
/// receiver, and the receiver must stay 2 px inside its screen.
#[test]
fn production_unit_test_sequence_matches_the_harness_models() {
    let events = [
        motion(10_000, 5.0, 0.0),
        motion(20_000, 7.0, 0.0),
        motion(30_000, -2.0, 0.0),
        motion(40_000, 4.0, 0.0),
        motion(50_000, 5.0, 0.0),
    ];

    let fixed = replay_events(
        SourceGuardKind::WindowOnly,
        Crossing::PeerLeft,
        ENTER_AT_ALONG,
        &events,
    );
    assert_eq!(
        fixed.forwarded.iter().map(|m| m.dx).collect::<Vec<_>>(),
        vec![-2.0],
        "only the crossing motion may reach the old receiver"
    );
    assert_eq!(fixed.old_raw_returns, 0);
    assert_eq!(fixed.old_tracker_returns, 0);
    assert_close(
        fixed.old_raw_outward_px,
        -2.0,
        "receiver outward position after the guarded sequence",
    );
    assert_eq!(
        fixed.first_forward_forwarded_at_us,
        Some(30_000),
        "the crossing motion must be forwarded at once"
    );

    // Without the source-side reverse guard the same raw sequence returns.
    let mut unguarded = OldReceiverModel::armed_at_entry_edge(EntryEdge::Right);
    for captured in events {
        unguarded.apply(captured.dx, captured.dy);
    }
    assert_eq!(
        unguarded.returns, 1,
        "the unguarded sequence must hand control back"
    );
}

/// The crossing must never feel stuck: the first forward delta is forwarded at
/// its capture timestamp (no batching/settle delay) and moves the remote
/// cursor into its screen immediately.
#[test]
fn window_only_guard_forwards_the_first_forward_immediately_and_moves_the_cursor_inward() {
    for scene in SCENES_GUARD_MUST_SUPPRESS
        .iter()
        .chain(std::iter::once(&SCENE_GENUINE_RETURN))
    {
        let report = replay(SourceGuardKind::WindowOnly, scene);
        let label = scene.name;

        let first_forward = scene
            .events
            .iter()
            .find(|m| scene.crossing.motion_kind(m.dx) == MotionKind::Forward)
            .expect("scene has a forward delta");
        assert_eq!(
            report.first_forward_forwarded_at_us,
            Some(first_forward.at_us),
            "{label}: the first forward delta must be forwarded immediately"
        );
        assert_eq!(
            report.forwarded.first().map(|m| m.dx),
            Some(first_forward.dx),
            "{label}: no delta may be forwarded ahead of the crossing motion"
        );
        let inward = report
            .inward_after_first_forward_px
            .expect("first forward was forwarded");
        assert_close(
            inward,
            first_forward.dx.abs(),
            &format!("{label}: inward distance after the first forward"),
        );
    }
}

/// A deliberate hand-back that starts after the fixed window must still be
/// delivered: the guard may only suppress the hand-off residual, never a real
/// return gesture.
#[test]
fn a_genuine_return_after_the_guard_window_is_forwarded() {
    let report = replay(SourceGuardKind::WindowOnly, &SCENE_GENUINE_RETURN);
    assert_eq!(
        report.dropped_reverse_px, 0.0,
        "a return after the window must not be suppressed"
    );
    assert_eq!(
        report.old_raw_returns, 1,
        "the old receiver must hand control back on the deliberate return"
    );
    assert_eq!(report.old_tracker_returns, 1);
    assert_eq!(
        report.old_raw_first_return_at_us,
        Some(250_000),
        "the return must fire on the post-window gesture"
    );

    // Boundary: the guard window is `now < until`; at exactly 200 ms the
    // window is over and the reverse delta is forwarded.
    let mut guard = SourceGuard::after_begin(SourceGuardKind::WindowOnly, Crossing::PeerRight);
    assert!(
        guard.on_motion(motion(199_999, -5.0, 0.0)).is_drop(),
        "199_999 us is still inside the 200 ms window"
    );
    assert_eq!(
        guard.on_motion(motion(200_000, -5.0, 0.0)),
        GuardDecision::Forward,
        "at exactly 200_000 us the window is over and a return must pass"
    );
}

/// The guard stays armed across forward deltas (this is exactly what the
/// current tree got wrong) but only for the fixed window.
#[test]
fn window_only_guard_stays_armed_after_forward_deltas_until_the_window_ends() {
    let mut guard = SourceGuard::after_begin(SourceGuardKind::WindowOnly, Crossing::PeerRight);

    // Crossing motion: forwarded, must not disarm the guard.
    assert_eq!(
        guard.on_motion(motion(1_000, 3.0247998237609863, 0.0)),
        GuardDecision::Forward
    );
    assert_eq!(
        guard.guard_until_us(),
        Some(HANDOFF_REVERSE_GUARD.as_micros() as u64),
        "forward motion must not move or clear the window deadline"
    );
    assert_eq!(guard.remaining_px(), Some(HANDOFF_RESIDUAL_MAX_PX));

    // Post-forward backswing: suppressed inside the window.
    let dropped = guard.on_motion(motion(2_000, -4.020392417907715, 0.0));
    assert!(
        dropped.is_drop(),
        "post-forward residual must be suppressed: {dropped:?}"
    );
    if let GuardDecision::DropReverse {
        outward_px,
        remaining_px,
        elapsed_us,
        guard_remaining_us,
    } = dropped
    {
        assert_close(outward_px, 4.020392417907715, "dropped outward distance");
        assert_close(
            remaining_px,
            HANDOFF_RESIDUAL_MAX_PX - 4.020392417907715,
            "remaining telemetry",
        );
        assert_eq!(elapsed_us, 2_000);
        assert_eq!(
            guard_remaining_us,
            200_000 - 2_000,
            "dropped deltas must not extend the fixed window"
        );
    }

    // Once the window is over the same reverse motion is forwarded.
    assert_eq!(
        guard.on_motion(motion(200_000, -4.020392417907715, 0.0)),
        GuardDecision::Forward
    );
    assert_eq!(guard.remaining_px(), None);
    assert_eq!(guard.guard_until_us(), None);
}

/// Only reverse motion is guarded: forward and perpendicular deltas are
/// forwarded untouched and never touch the reverse telemetry.
#[test]
fn window_only_guard_only_filters_reverse_motion() {
    let mut guard = SourceGuard::after_begin(SourceGuardKind::WindowOnly, Crossing::PeerRight);

    // Forward motion with a large perpendicular component.
    assert_eq!(
        guard.on_motion(motion(500, 2.5, 40.0)),
        GuardDecision::Forward
    );
    // Perpendicular-only motion is neither forward nor reverse.
    assert_eq!(
        guard.on_motion(motion(600, 0.0, 40.0)),
        GuardDecision::Forward
    );
    assert_eq!(
        guard.remaining_px(),
        Some(HANDOFF_RESIDUAL_MAX_PX),
        "forward/neutral motion must not touch the reverse telemetry"
    );

    // A reverse delta is the only thing the guard ever drops.
    assert!(guard.on_motion(motion(700, -3.0, 0.0)).is_drop());
    assert_close(
        guard.remaining_px().expect("guard armed"),
        HANDOFF_RESIDUAL_MAX_PX - 3.0,
        "reverse telemetry after a drop",
    );
}

/// Inside the fixed window every reverse delta is dropped, however large, and
/// the 32 px telemetry saturating at 0 never ends the guard. After the window
/// the same reverse motion passes (delayed by at most the window, never lost).
#[test]
fn energetic_reverse_pushes_inside_the_window_are_all_suppressed() {
    let mut guard = SourceGuard::after_begin(SourceGuardKind::WindowOnly, Crossing::PeerRight);

    for at_us in [1_000, 2_000, 3_000, 4_000] {
        assert!(
            guard.on_motion(motion(at_us, -20.0, 0.0)).is_drop(),
            "reverse push at {at_us} us must be suppressed"
        );
    }
    assert_eq!(
        guard.remaining_px(),
        Some(0.0),
        "the trace distance saturates at 0"
    );
    assert_eq!(
        guard.guard_until_us(),
        Some(HANDOFF_REVERSE_GUARD.as_micros() as u64),
        "the guard is bounded by the fixed window, not by accumulated distance"
    );

    // Delayed, not lost: after the window the reverse motion passes.
    assert_eq!(
        guard.on_motion(motion(200_000, -20.0, 0.0)),
        GuardDecision::Forward
    );
    assert_eq!(guard.remaining_px(), None);
    assert_eq!(guard.guard_until_us(), None);
}

/// The active trace's single `-63.27 px` reverse warp (serial 3) used to be
/// forwarded whole by the superseded 32 px budget rule. The window-only guard
/// drops it, the forward correction crosses immediately, and the old receiver
/// stays inside its screen; the unguarded sequence is shown to return.
#[test]
fn serial3_single_63px_reverse_warp_is_suppressed_inside_the_window() {
    let warp = motion(127, -63.27177047729492, 13.754733085632324);
    let correction = motion(48_000, 65.33959197998047, -11.189517974853516);

    let mut guard = SourceGuard::after_begin(SourceGuardKind::WindowOnly, Crossing::PeerRight);
    assert!(
        guard.on_motion(warp).is_drop(),
        "the single 63.27 px warp must be suppressed inside the window"
    );
    assert_eq!(guard.remaining_px(), Some(0.0), "telemetry saturates");
    assert_eq!(
        guard.guard_until_us(),
        Some(HANDOFF_REVERSE_GUARD.as_micros() as u64),
        "the large warp must not end the guard"
    );
    assert_eq!(
        guard.on_motion(correction),
        GuardDecision::Forward,
        "the forward correction must be forwarded immediately"
    );

    let mut receiver = OldReceiverModel::armed_at_entry_edge(EntryEdge::Left);
    receiver.apply(correction.dx, correction.dy);
    assert_eq!(
        receiver.returns, 0,
        "only the forward correction may reach the old receiver"
    );
    assert_close(
        receiver.outward_px,
        -65.33959197998047,
        "old receiver cursor stays inside its screen",
    );

    // Sensitivity check: without the guard the same raw sequence returns.
    let mut unguarded = OldReceiverModel::armed_at_entry_edge(EntryEdge::Left);
    unguarded.apply(warp.dx, warp.dy);
    unguarded.apply(correction.dx, correction.dy);
    assert_eq!(
        unguarded.returns, 1,
        "the unguarded serial-3 sequence must hand control back"
    );
}

/// The active trace's serial-2 pattern (`-17.96` activation burst, `+12.39`
/// forward, then the `-16.94` coalesced warp; reverse sum `34.9 px > 32 px`)
/// must not leak any reverse delta to the old receiver anymore, and the
/// unguarded version still returns (sensitivity check).
#[test]
fn serial2_34_9px_burst_is_suppressed_and_old_receiver_stays_inward() {
    let report = replay(SourceGuardKind::WindowOnly, &SCENE_SERIAL2_BURST);
    let burst_px = 17.960790634155273 + 16.937875747680664;
    assert_close(
        report.dropped_reverse_px,
        burst_px,
        "every reverse delta of the serial-2 burst must be dropped",
    );
    assert_eq!(
        report.forwarded.iter().map(|m| m.dx).collect::<Vec<_>>(),
        vec![12.39034652709961],
        "only the forward delta may reach the old receiver"
    );
    assert_eq!(report.old_raw_returns, 0);
    assert_eq!(report.old_tracker_returns, 0);
    assert_close(
        report.old_raw_outward_px,
        -12.39034652709961,
        "old receiver cursor stays inside its screen",
    );

    // Unguarded sensitivity check: the raw serial-2 pattern returns.
    let mut unguarded = OldReceiverModel::armed_at_entry_edge(EntryEdge::Left);
    for captured in SCENE_SERIAL2_BURST_EVENTS {
        unguarded.apply(captured.dx, captured.dy);
    }
    assert_eq!(
        unguarded.returns, 1,
        "the unguarded serial-2 pattern must hand control back"
    );
}

// ===========================================================================
// 8. Tests: guard does not delay forwarding
// ===========================================================================

/// Every forward delta of the corpus is forwarded, unchanged and in order, for
/// both guards (the reverse filter is direction-only; it is not a settle).
#[test]
fn no_guard_delays_or_drops_forward_deltas() {
    for scene in ALL_SCENES {
        for kind in [SourceGuardKind::CurrentTree, SourceGuardKind::WindowOnly] {
            let report = replay(kind, scene);
            let expected_forwards: Vec<(u64, f64)> = scene
                .events
                .iter()
                .filter(|m| scene.crossing.motion_kind(m.dx) == MotionKind::Forward)
                .map(|m| (m.at_us, m.dx))
                .collect();
            let forwarded_forwards: Vec<(u64, f64)> = report
                .forwarded
                .iter()
                .filter(|m| scene.crossing.motion_kind(m.dx) == MotionKind::Forward)
                .map(|m| (m.at_us, m.dx))
                .collect();
            assert_eq!(
                forwarded_forwards, expected_forwards,
                "{:?} {}: forward deltas must be forwarded unchanged and in order",
                kind, scene.name
            );
        }
    }
}

/// The first movement of a hand-off must not wait for the coalescer's batch
/// window (the "cursor sticks at the edge" symptom). The production
/// `MotionCoalescer` is compiled in read-only from `src/coalesce.rs`, exactly
/// like the existing verifier harness does.
#[test]
fn coalescer_flushes_the_first_forward_of_a_handoff_immediately() {
    // A deliberately huge batch window: the assertion must be about the
    // hand-off burst, not about the normal 2-4 ms timer.
    let mut coalescer = MotionCoalescer::new(60_000);
    let t0 = Instant::now();
    coalescer.push(HANDLE, 0, 0.5, 0.0, t0);
    assert!(
        coalescer.take(HANDLE).is_some(),
        "flush motion from before the hand-off"
    );

    coalescer.arm_leading_edge();
    let first_forward = SCENE_SLOW
        .events
        .iter()
        .find(|m| SCENE_SLOW.crossing.motion_kind(m.dx) == MotionKind::Forward)
        .expect("scene has a forward delta");
    assert!(
        coalescer.push(
            HANDLE,
            1,
            first_forward.dx,
            first_forward.dy,
            t0 + Duration::from_micros(100)
        ),
        "the first forward delta after Begin must be flushed immediately"
    );
    let flushed = coalescer.take(HANDLE).expect("first forward flushed");
    assert_close(flushed.dx, first_forward.dx, "flushed first forward dx");
    assert_close(flushed.dy, first_forward.dy, "flushed first forward dy");

    // The hand-off burst keeps the following motion immediate too.
    assert!(
        coalescer.push(HANDLE, 2, 0.0, 1.0, t0 + Duration::from_micros(200)),
        "the initial crossing burst must also bypass batching"
    );
}

// ===========================================================================
// 9. Deterministic sweep over the observed delta quanta
// ===========================================================================

/// Forward deltas observed in the trace (all positive: peer to the right).
const OBSERVED_FORWARDS_PX: &[f64] = &[
    0.4580078125,
    0.8613455891609192,
    1.5123999118804932,
    3.0247998237609863,
    12.39034652709961,
];

/// Reverse magnitudes observed in the trace.
const OBSERVED_REVERSE_PX: &[f64] = &[
    1.4109896421432495,
    4.020392417907715,
    9.0743989944458,
    13.61159896850586,
    16.937875747680664,
    63.27177047729492,
];

/// A deterministic grid over the observed quanta: the current tree fires the
/// old receiver exactly when the leaked residual is larger than the forward
/// motion plus the 3 px margin; the final window-only guard suppresses every
/// combination inside the fixed window, however large. This cross-checks both
/// models beyond the hand-written scenes.
#[test]
fn observed_delta_grid_matches_the_contract() {
    let leading = 1.4109896421432495;
    let mut current_tree_fires = 0;
    let mut current_tree_quiet = 0;

    for &forward in OBSERVED_FORWARDS_PX {
        for &residual in OBSERVED_REVERSE_PX {
            let events: [Motion; 3] = [
                motion(100, -leading, 0.0),
                motion(200, forward, 0.0),
                motion(300, -residual, 0.0),
            ];

            // Current tree: the leading reverse is dropped, the first forward
            // disarms the filter, the residual is forwarded whole.
            let legacy = replay_events(
                SourceGuardKind::CurrentTree,
                Crossing::PeerRight,
                ENTER_AT_ALONG,
                &events,
            );
            let expected_return = residual - forward > RETURN_MARGIN_PX;
            assert_eq!(
                legacy.old_raw_returns,
                u32::from(expected_return),
                "current tree, forward {forward}, residual {residual}: raw model"
            );
            assert_eq!(
                legacy.old_tracker_returns,
                u32::from(expected_return),
                "current tree, forward {forward}, residual {residual}: faithful tracker"
            );
            if expected_return {
                current_tree_fires += 1;
            } else {
                current_tree_quiet += 1;
            }

            // Final window-only guard: everything inside the window is
            // suppressed, including combinations whose reverse sum exceeds the
            // old 32 px telemetry bound (the grid includes 63.27 px bursts).
            let fixed = replay_events(
                SourceGuardKind::WindowOnly,
                Crossing::PeerRight,
                ENTER_AT_ALONG,
                &events,
            );
            assert_eq!(
                fixed.old_raw_returns, 0,
                "window-only, forward {forward}, residual {residual}: raw model"
            );
            assert_eq!(
                fixed.old_tracker_returns, 0,
                "window-only, forward {forward}, residual {residual}: faithful tracker"
            );
            assert_close(
                fixed.dropped_reverse_px,
                leading + residual,
                "window-only grid dropped reverse distance",
            );
            assert_eq!(
                fixed.forward_events, fixed.forward_events_forwarded,
                "window-only grid forward deltas"
            );
        }
    }

    // The sweep must actually exercise both outcomes, otherwise it would pass
    // vacuously.
    assert!(
        current_tree_fires > 0 && current_tree_quiet > 0,
        "grid must contain both leaking and non-leaking old-receiver cases"
    );
}

// ===========================================================================
// 10. Model/implementation parity and trace provenance
// ===========================================================================

/// Keep the harness model honest: the constants and trace fields it
/// transcribes must still be present in `src/capture.rs`. This is a cheap
/// static sync check, not a replacement for the verifier's behavioural tests.
#[test]
fn harness_guard_model_matches_the_production_guard() {
    assert_eq!(HANDOFF_RESIDUAL_MAX_PX, 32.0);
    assert_eq!(HANDOFF_REVERSE_GUARD, Duration::from_millis(200));

    for needle in [
        "const HANDOFF_RESIDUAL_MAX_PX: f64 = 32.0;",
        "const HANDOFF_REVERSE_GUARD: Duration = Duration::from_millis(200);",
        "fn handoff_outward_px(pos: Position, dx: f64, dy: f64) -> f64",
        "reverse_guard_until: Option<Instant>",
        "self.reverse_guard_until = Some(handoff_began + HANDOFF_REVERSE_GUARD);",
        "self.residual_reverse_px = Some(HANDOFF_RESIDUAL_MAX_PX);",
        // Final (task-7) window-only decision: the delta is dropped whole and
        // only the *telemetry* saturates at 0.
        "The window is the only decision",
        "let left_px = (remaining_px - reverse_px).max(0.0);",
        "\"guard_remaining_us\"",
    ] {
        assert!(
            CAPTURE_SOURCE.contains(needle),
            "src/capture.rs no longer contains {needle:?}: the harness model \
             and the production guard have drifted"
        );
    }

    // The superseded budget decision must stay gone: the window alone decides,
    // so a reverse delta can never be forwarded just because it is large.
    assert!(
        !CAPTURE_SOURCE.contains("reverse_px < remaining_px"),
        "the superseded 32 px budget decision (forward reverse deltas past the \
         budget) is back in src/capture.rs"
    );

    // The bug this harness characterises must stay fixed: the guard may not be
    // disarmed by the first forward delta.
    assert!(
        !CAPTURE_SOURCE.contains("The crossing motion itself: stop filtering so a later"),
        "the current-tree behaviour (clear the residual filter on the first \
         forward delta) is back in src/capture.rs"
    );
}

/// The magnitudes in [`Scene`]s are expanded from real trace records. When the
/// local trace file exists, every cited record must still be present; when it
/// does not (CI, a fresh machine) the check is skipped.
#[test]
fn corpus_is_grounded_in_the_local_handoff_trace() {
    let files = existing_trace_files();
    if files.is_empty() {
        eprintln!(
            "skipped: no handoff trace at ~/.cache/lan-mouse/handoff-trace.jsonl \
             (set LAN_MOUSE_HANDOFF_TRACE_FILE to audit the corpus elsewhere)"
        );
        return;
    }

    let mut lines: Vec<String> = Vec::new();
    for path in &files {
        match fs::read_to_string(path) {
            Ok(text) => lines.extend(text.lines().map(str::to_owned)),
            Err(error) => eprintln!("warning: reading {} failed: {error}", path.display()),
        }
    }

    let mut missing: Vec<String> = Vec::new();
    for scene in ALL_SCENES {
        for basis in scene.basis {
            if !lines.iter().any(|line| trace_line_matches(line, basis)) {
                missing.push(format!(
                    "{}: stage={} serial={} dx={} dy={}",
                    scene.name, basis.stage, basis.serial, basis.dx, basis.dy
                ));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "corpus cites trace records that are no longer in the local trace \
         files; re-ground the corpus and update \
         docs/testing/handoff-old-receiver-harness.md:\n{}",
        missing.join("\n")
    );
    eprintln!(
        "trace provenance: {} basis records found in {} trace file(s)",
        ALL_SCENES.iter().map(|s| s.basis.len()).sum::<usize>(),
        files.len()
    );
}

/// Resolve the trace files the Lan Mouse source writes
/// (`docs/testing/handoff-trace.md`): the active file plus its single rotated
/// backup. Returns only paths that exist.
fn existing_trace_files() -> Vec<PathBuf> {
    let primary = std::env::var_os("LAN_MOUSE_HANDOFF_TRACE_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
                .map(|cache| cache.join("lan-mouse").join("handoff-trace.jsonl"))
        });
    let Some(primary) = primary else {
        return Vec::new();
    };
    let rotated = PathBuf::from(format!("{}.1", primary.display()));
    [primary, rotated]
        .into_iter()
        .filter(|path| path.is_file())
        .collect()
}

/// Does one JSONL trace line carry exactly this observed record?
fn trace_line_matches(line: &str, basis: &TraceBasis) -> bool {
    let Some(stage) = json_string_field(line, "stage") else {
        return false;
    };
    if stage != basis.stage {
        return false;
    }
    if json_u64_field(line, "serial") != Some(u64::from(basis.serial)) {
        return false;
    }
    let dx = json_string_field(line, "dx").and_then(|value| value.parse::<f64>().ok());
    let dy = json_string_field(line, "dy").and_then(|value| value.parse::<f64>().ok());
    matches!((dx, dy), (Some(dx), Some(dy)) if (dx - basis.dx).abs() <= 1e-6 && (dy - basis.dy).abs() <= 1e-6)
}

/// Value of a compact-JSON string field (`{"a":"v"}`), without a JSON crate:
/// the trace schema is flat and every value the harness reads is a string.
fn json_string_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{key}\":\"");
    let start = line.find(&pattern)? + pattern.len();
    let end = start + line[start..].find('"')?;
    Some(&line[start..end])
}

/// Value of a compact-JSON integer field (`{"a":7}`).
fn json_u64_field(line: &str, key: &str) -> Option<u64> {
    let pattern = format!("\"{key}\":");
    let start = line.find(&pattern)? + pattern.len();
    let rest = &line[start..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}
