# Old-Windows receiver hand-off harness (independent QA)

Independent, deterministic compatibility harness for the Ubuntu (source) ->
**old Windows 0.2.18 receiver** (`5233b05`) hand-off. It replays realistic
source delta sequences derived from the local hand-off trace through a model of
the production reverse-motion guard and the old receiver's absolute pointer
tracker, and answers one question deterministically: **would the old receiver
ask for the capture back (`LeaveAt`) on this sequence?**

This document belongs to task-8 (reconciling task-5's harness with the final
window-only guard from task-7). It records the receiver model, the trace basis
of the replay corpus, the exact commands/results, the pass/fail evidence for
the current tree vs. the final guard, and the remaining risks. No production
file was edited; the only files owned by this harness are
`tests/qa_handoff_old_receiver.rs` and this document.

## Verdict

**PASS** for the final window-only guard (task-7). Inside the fixed 200 ms
post-`Begin` window every reverse delta is dropped **regardless of size**;
`HANDOFF_RESIDUAL_MAX_PX = 32 px` is trace telemetry only (it saturates at 0 and
never ends the guard); forward deltas stay immediate; reverse motion passes
again once the window ends. The pre-fix source guard fails the same corpus.

| gate | result |
|---|---|
| `cargo test --test qa_handoff_old_receiver` | **PASS** — 31/31 (17 harness tests + 14 `src/coalesce.rs` unit tests compiled in read-only) |
| `cargo clippy --test qa_handoff_old_receiver -- -D warnings` | **PASS** — no warnings |
| `rustfmt --edition 2021 --check tests/qa_handoff_old_receiver.rs` | **PASS** — no diff |
| current tree @ `8b6981d` replay (model of the old guard) | **FAIL** — old receiver returns on all 5 reverse scenes, under both receiver models |
| final window-only guard replay of the same corpus | **PASS** — 0 returns, 0 reverse deltas forwarded inside the guard, 5/5 scenes |
| serial-2 `34.9 px` burst (`-17.96` + `-16.94`) | **PASS** — both reverse deltas dropped whole, old receiver stays `12.39 px` inside |
| serial-3 single `-63.27 px` warp | **PASS** — dropped whole, old receiver stays `65.34 px` inside |
| first forward delta immediate | **PASS** — forwarded at its capture timestamp; moves the old cursor inward by the full delta |
| genuine return after the 200 ms window | **PASS** — forwarded, old receiver hands control back |
| trace-provenance of the corpus | **PASS** — 18/18 cited trace records found in the local trace files |
| production guard unit tests (`cargo test -p lan-mouse --lib capture_handle_tests`) | **PASS** — 17/17 in the module; the 9 guard tests are cross-referenced below (task-7) |

## 1. Scope and environment

| item | value |
|---|---|
| repository | `/home/gabriel/code/lan-mouse-custom` |
| base commit | `8b6981d` (`main`, the tree the user reports the bug on) |
| working tree | `src/capture.rs` modified by task-4 and finalised by task-7 (window-only reverse guard) |
| verified `src/capture.rs` | sha256 `a1a3909f35078511532a17744c09070298d21e60fa930045fd94a19248ede640` (`git hash-object` `bd8627a4005702525e58b05505578d0d3d4ab3fd`); diff vs `8b6981d`: 497 insertions / 171 deletions |
| added by this harness | `tests/qa_handoff_old_receiver.rs`, `docs/testing/handoff-old-receiver-harness.md` |
| host | Ubuntu 26.04, Linux 7.0.0, x86_64 |
| toolchain | `rustc 1.99.0` (2026-09-28), `cargo 1.99.0` |
| date | 2026-11-13 |
| trace files read | `~/.cache/lan-mouse/handoff-trace.jsonl` (active) and `.jsonl.1` (rotated backup) |

The harness is layer L2: pure functions, virtual microsecond timestamps, no
sockets, no sleeps, no display. It does not import the private capture
internals; the production state machine is modelled as pure functions and the
model is pinned to the implementation by a static sync test (§4.3).

## 2. Old receiver model (release `5233b05`)

The task/contract model is the release `5233b05` absolute tracker:

- `input-emulation/src/libei.rs` @ `5233b05`:
  `const RETURN_MARGIN_PX: f64 = 3.0;` (line 145);
  `AbsoluteTracker::apply` (line 382) computes the **raw** position from the
  tracked position plus the injected delta and fires the hand-off only when
  *both* the snapped position is still in the entry-edge band (`near_edge`) and
  the raw position has passed the edge by the margin (`past_edge`, line 434):
  `Left => raw_x < x0 - 3.0`.
- `EnterAt` anchors the pointer **on the entry edge itself**:
  `entry_point_at` (line 555) returns `(region.x0, along)` for a left edge and
  `(region.x1 - 1.0, along)` for a right edge. `EDGE_ANCHOR_INSET = 24 px` only
  applies to legacy peers that do not send `EnterAt`.
- The observed trace always has the source controller at `pos: Some(Right)`
  (the peer screen is to the right of the Ubuntu screen), so the old receiver is
  entered through its **left** edge and a return fires once the raw pointer
  travels more than 3 px *out* of that edge.

Two models are implemented and both must agree on every replay:

1. `OldReceiverModel` — the behavioural contract used by the production unit
   test: the entry anchor is `outward_px = 0`, every forwarded delta moves the
   raw outward position (`Position::Right` source => receiver outward delta
   `-dx`), and a return fires when `outward_px > 3.0`. This is the model named
   in the task.
2. `OldAbsoluteTracker` — a faithful transcription of
   `AbsoluteTracker::apply` @ `5233b05` for a single 1920x1080 monitor: the
   injected position is clamped/snapped into the entry region (as the
   compositor does), `near_edge` is checked on the snapped position, `past_edge`
   on the raw position, the tracker snaps to the edge when it fires and latches
   `fired` until the pointer is 20 px inside the screen again. This model is
   strictly closer to the real old receiver and is an independent oracle: a
   pass/fail claim is only made when both models agree.

For reference, the release `5233b05` **Windows backend** uses the same contract
through `input-emulation/src/absolute.rs`: `EDGE_RETURN_MIN_OVERSHOOT = 2` over
the last pixel inside the desktop, a 1 px `EnterAt` anchor inset and a per-move
overshoot check. It is at least as sensitive as the contract model near the
edge. The harness models the absolute-tracker contract requested by the task
(and the faithful libei tracker at the same release); since both pass scenes
suppress *every* reverse delta before it reaches the receiver, the small
geometry difference cannot change the pass/fail result. A real old-Windows
manual check remains in §9.

## 3. Replay corpus (trace basis)

The trace records the **coalesced** motion the source sent, not the individual
capture events: `capture.first_motion_sent` carries one `dx`/`dy` and
`capture.reverse_motion_dropped` one aggregate reverse delta. The old receiver
consumes exactly those coalesced sender packets, so the corpus replays each
observed aggregate as one receiver-visible event and only the *sequence
placement* (leading residual vs. post-forward residual vs. return after the
window) is reconstructed. The provenance test (§6) checks all 18 cited records
against the local trace files.

| scene | replay deltas, in capture order | why it is realistic |
|---|---|---|
| `slow_crossing_then_warp_residual` | `-1.411`, `+0.458`, `-4.020`, `+0.867` px | tiny first forward (slow touchpad) followed by a compositor warp |
| `medium_crossing_then_full_backswing` | `-3.884`, `+3.025`, `-9.074`, `+1.512` px | first forward then a full backswing residual |
| `trace_serial2_burst_then_residual` | `-17.961`, `+3.025`, `-9.074` px | the active trace's `serial 2` activation aggregate, then a post-forward residual |
| `deliberate_return_after_the_guard` | `+3.025` at 1 ms, `-9.074` at **250 ms** | a genuine hand-back after the 200 ms window |
| `serial2_34_9px_burst` | `-17.961`, `+12.390`, `-16.938` px | the full active-trace `serial 2` shape; reverse sum `34.9 px > 32 px` (the old-budget hole task-7 closed) |
| `serial3_63_27px_single_warp` | `-63.272` at 127 us, `+65.340` at 48 ms | the active trace's single largest reverse warp (`serial 3`) and the `serial 4` forward correction |

Trace provenance (`ts_unix_us` is the canonical id; line numbers shift while
the trace is live):

| value | record | location |
|---|---|---|
| `-1.4109896421432495` | `capture.reverse_motion_dropped` serial 7 | `.jsonl.1`, ts `1794580650358339` |
| `+0.4580078125 / +1.3053388595581055` | `capture.first_motion_sent` serial 19 | `.jsonl.1`, ts `1794430623084772` |
| `-4.020392417907715 / +2.1487011909484863` | `capture.first_motion_sent` serial 20 | `.jsonl.1`, ts `1794430625640281` |
| `+0.8673844337463379` | `capture.first_motion_sent` serial 8 | active, ts `1794581849908221` |
| `-3.8841233253479004 / +4.8841233253479` | `capture.first_motion_sent` serial 25 | `.jsonl.1`, ts `1794430629481824` |
| `+3.0247998237609863` | `capture.first_motion_sent` serial 2 | active, ts `1794581842549740` |
| `-9.0743989944458 / +9.0743989944458` | `capture.first_motion_sent` serial 4 | `.jsonl.1`, ts `1794430610955580` |
| `+1.5123999118804932` | `capture.first_motion_sent` serial 9 | active, ts `1794581851318772` |
| `-17.960790634155273 / +1.6327991485595703` | `capture.reverse_motion_dropped` serial 2 | active, ts `1794593003310021` |
| `+12.39034652709961 / -1.1390466690063477` | `capture.first_motion_sent` serial 2 | active, ts `1794593003310034` |
| `-16.937875747680664 / +4.619420528411865` | `capture.reverse_motion_dropped` serial 2 | `.jsonl.1`, ts `1794578609451935` |
| `-63.27177047729492 / +13.754733085632324` | `capture.first_motion_sent` serial 3 | active, ts `1794593004124267` |
| `+65.33959197998047 / -11.189517974853516` | `capture.first_motion_sent` serial 4 | active, ts `1794593004138477` |

The active trace contains the churn signature this task is about: the same
session sends the reverse aggregate `-17.961 px`, then the forward
`+12.390 px`, and ~814 ms later a new session's first motion is `-63.272 px`.
Both reverse bursts are exactly the material the final guard now drops whole
inside the 200 ms window.

## 4. Source guards compared

### 4.1 Current tree (`8b6981d`) - FAILS

`src/capture.rs` @ `8b6981d` drops reverse deltas only until the **first
forward delta**, then disarms the filter:

```rust
if reverse_px <= 0.0 {
    // The crossing motion itself: stop filtering so a later
    // return push is forwarded normally.
    self.residual_reverse_px = None;
} else if reverse_px < remaining_px { /* drop */ }
```

The touchpad backswing arrives *after* the crossing motion, so its residual is
forwarded to the old receiver. In the replay corpus the receiver is pushed
`3.562 px` (slow scene), `6.050 px` (medium scene), `6.050 px` (trace-burst
scene), `4.548 px` (serial-2 burst) and `63.272 px` (serial-3 single warp) past
its entry edge: all five exceed `RETURN_MARGIN_PX = 3.0` and fire a `LeaveAt`
in both receiver models.

The serial-2 and serial-3 shapes are also why the superseded intermediate guard
(which forwarded a reverse delta at/over the 32 px budget whole as a
"deliberate hand-back") was not enough: those bursts exceed the nominal 32 px
bound and still reached the receiver. Task-7 replaced that decision with the
fixed window, which the harness now models.

### 4.2 Final window-only guard (task-7) - PASSES

`src/capture.rs` at the verified sha256 above:

- `HANDOFF_REVERSE_GUARD = 200 ms`, fixed from `Begin`, never extended by
  dropped deltas;
- the fixed window is the **only** decision: every reverse delta inside it is
  dropped **regardless of accumulated size**;
- `HANDOFF_RESIDUAL_MAX_PX = 32 px` is **trace telemetry only**: it seeds the
  `remaining_px` field, which decreases with every dropped delta, saturates at
  `0` and never ends the guard;
- **only** reverse motion is filtered; forward deltas are always forwarded
  immediately and never disarm the guard or consume the telemetry;
- the window is checked lazily on the next pointer event
  (`now < until`), so at exactly 200 ms the window is over and reverse motion
  is forwarded unchanged (delayed by at most the window, never lost);
- trace stage `capture.reverse_motion_dropped` carries `dx`, `dy`,
  `remaining_px` (saturating telemetry), `elapsed_us` and `guard_remaining_us`.

Why the window-only rule: the serial-3 trace shows a single coalesced reverse
warp of `-63.27 px`, and the serial-2 shape accumulates `34.9 px` of reverse
motion. The superseded budget rule forwarded any delta at/over the 32 px bound
whole as a "deliberate hand-back", which is exactly how the old receiver could
still fire one `LeaveAt` and keep the source cursor visible at the border
(originally reported by task-5's harness; fixed by task-7).

### 4.3 Model parity

`SourceGuardKind::WindowOnly` in the harness is a transcription of that block;
the test `harness_guard_model_matches_the_production_guard` pins the two
constants, the `handoff_outward_px` helper, the `reverse_guard_until` field and
its `Begin` assignment, the `guard_remaining_us` trace field, the
`left_px = (remaining_px - reverse_px).max(0.0)` telemetry saturation and the
"The window is the only decision" comment against `src/capture.rs` (compiled
into the test with `include_str!`). It also fails if the superseded
`reverse_px < remaining_px` budget decision or the pre-fix
"stop filtering on the first forward delta" behaviour return. Behavioural
parity is additionally covered by the task-7 production unit tests and the
task-6 verification.

## 5. Replay results

`old raw` = contract model (`RETURN_MARGIN_PX = 3.0`, raw outward position);
`tracker` = faithful `5233b05 AbsoluteTracker`. `returns` is the number of
`LeaveAt` requests the old receiver would have sent.

| scene | current tree: returns (outward at return) | old raw | tracker | final guard: dropped reverse | old raw | tracker |
|---|---|---|---|---|---|---|
| slow crossing + warp | 1 @5 ms (`3.562 px`) | **1** | **1** | 5.431 px | 0 | 0 |
| medium + full backswing | 1 @6 ms (`6.050 px`) | **1** | **1** | 12.959 px | 0 | 0 |
| trace serial-2 burst + residual | 1 @7 ms (`6.050 px`) | **1** | **1** | 27.035 px | 0 | 0 |
| serial-2 `34.9 px` burst | 1 @1 ms (`4.548 px`) | **1** | **1** | 34.899 px (telemetry saturates 0) | 0 | 0 |
| serial-3 `63.27 px` single warp | 1 @127 us (`63.272 px`) | **1** | **1** | 63.272 px (telemetry saturates 0) | 0 | 0 |
| deliberate return after window | 1 @250 ms | 1 | 1 | 0 px | 1 @250 ms | 1 @250 ms |

The final-guard rows are exactly the contract: **no reverse delta reaches the
old receiver inside the guard - not even a single 63.27 px warp or a 34.9 px
burst** - the first forward delta moves the remote cursor inward immediately
(`0.458 / 3.025 / 12.390 / 65.340 px` at its capture timestamp), and a
deliberate return that starts after the window still reaches the receiver.

Raw evidence printed by the harness (`--nocapture`, abridged):

```text
[current-tree] slow_crossing_then_warp_residual: raw outward at return = 3.562 px, returned at Some(5000) us
[current-tree] serial2_34_9px_burst: raw outward at return = 4.548 px, returned at Some(1000) us
[current-tree] serial3_63_27px_single_warp: raw outward at return = 63.272 px, returned at Some(127) us
[window-only] slow_crossing_then_warp_residual: dropped 5.431 px, forwarded [0.4580078125, 0.8673844337463379], receiver outward = -1.325 px
[window-only] serial2_34_9px_burst: dropped 34.899 px, telemetry [remaining 14.039, remaining 0.0], forwarded [12.39034652709961], receiver outward = -12.390 px
[window-only] serial3_63_27px_single_warp: dropped 63.272 px, telemetry [remaining 0.0], forwarded [65.33959197998047], receiver outward = -65.340 px
```

## 6. Test inventory (`tests/qa_handoff_old_receiver.rs`)

| test | asserts |
|---|---|
| `old_receiver_contract_model_fires_past_the_three_pixel_margin` | `EnterAt` anchor on the edge; return only past 3.0 px; latch |
| `old_absolute_tracker_agrees_with_the_contract_model` | faithful tracker anchor/position; forward then residual return |
| `current_tree_source_guard_leaks_the_post_forward_residual` | 5/5 scenes: reverse forwarded, both receiver models return past the margin |
| `window_only_source_guard_keeps_the_old_receiver_from_returning` | 5/5 scenes: 0 reverse forwarded, 0 returns, every reverse delta dropped, all forward deltas forwarded |
| `production_unit_test_sequence_matches_the_harness_models` | independent re-derivation of production's `+5, +7, -2, +4, +5` sequence: only `-2` reaches the receiver, outward `-2 px`, no return |
| `window_only_guard_forwards_the_first_forward_immediately_and_moves_the_cursor_inward` | first forward at its capture timestamp, full inward displacement |
| `a_genuine_return_after_the_guard_window_is_forwarded` | return at 250 ms forwarded; boundary `199_999 us` drops vs `200_000 us` forwards |
| `window_only_guard_stays_armed_after_forward_deltas_until_the_window_ends` | forward does not disarm; deadline fixed; telemetry tracked; trace fields |
| `window_only_guard_only_filters_reverse_motion` | forward/perpendicular deltas forwarded and telemetry untouched |
| `energetic_reverse_pushes_inside_the_window_are_all_suppressed` | four `-20 px` pushes all dropped; telemetry saturates at 0; window still armed; reverse after window passes |
| `serial3_single_63px_reverse_warp_is_suppressed_inside_the_window` | single `-63.27 px` warp dropped; forward correction immediate; old receiver stays inside; unguarded sequence returns |
| `serial2_34_9px_burst_is_suppressed_and_old_receiver_stays_inward` | full serial-2 shape: all `34.9 px` reverse dropped; only `+12.39` reaches the receiver; unguarded pattern returns |
| `no_guard_delays_or_drops_forward_deltas` | every forward delta of every scene forwarded unchanged/in order, both guards |
| `coalescer_flushes_the_first_forward_of_a_handoff_immediately` | real `src/coalesce.rs` leading-edge flush, no batch delay |
| `observed_delta_grid_matches_the_contract` | deterministic sweep 5 observed forwards x 6 observed reverse quanta (30 combinations, including `63.27 px`): current tree returns iff `residual > forward + 3 px`; final guard never returns and drops everything |
| `harness_guard_model_matches_the_production_guard` | model/source parity (constants, helper, telemetry saturation, window-only comment; superseded budget/pre-fix behaviour absent) |
| `corpus_is_grounded_in_the_local_handoff_trace` | every cited trace record still present (skips when no trace exists) |

The 14 extra tests in the run are `src/coalesce.rs`'s own unit tests, compiled
in read-only through `#[path]` exactly like `tests/qa_handoff_shutdown.rs` does;
they are not part of this harness's assertions.

## 7. Commands and results

```sh
cd /home/gabriel/code/lan-mouse-custom

# Harness (also prints the replay evidence with --nocapture)
cargo test --test qa_handoff_old_receiver
# -> running 31 tests; test result: ok. 31 passed; 0 failed

cargo test --test qa_handoff_old_receiver -- --nocapture --test-threads=1
# -> [current-tree]/[window-only] evidence lines quoted in section 5

cargo clippy --test qa_handoff_old_receiver -- -D warnings
# -> Finished `dev` profile ... (no warnings)

rustfmt --edition 2021 --check tests/qa_handoff_old_receiver.rs
# -> no diff

# Cross-reference: the final production guard tests (task-7)
cargo test -p lan-mouse --lib capture_handle_tests
# -> 17 passed; the 9 guard tests are:
#    a_large_reverse_burst_inside_the_window_is_never_forwarded,
#    a_reverse_return_after_the_guard_window_is_forwarded,
#    a_single_large_reverse_warp_inside_the_window_is_suppressed,
#    forward_motion_is_never_delayed_or_dropped_by_the_reverse_guard,
#    reverse_residual_after_the_first_forward_is_suppressed_until_the_guard_ends,
#    the_first_forward_motion_after_a_handoff_is_forwarded_immediately,
#    the_leading_reverse_burst_after_a_handoff_is_suppressed,
#    the_reverse_guard_keeps_an_old_receiver_from_handing_control_back,
#    the_reverse_guard_stays_short_and_small
```

No push, CI job or release was triggered; everything is local-only.

## 8. Residual risks and follow-ups

### 8.1 Window-only trade-off: an immediate deliberate hand-back is delayed

Inside the 200 ms window *all* reverse motion is treated as activation
residual, including a determined reverse push started immediately after the
crossing. Such a push is not forwarded until the window ends, so an intentional
hand-back begun in the first 200 ms is delayed by at most 200 ms - it is never
lost (§4.2, `a_genuine_return_after_the_guard_window_is_forwarded`). That delay
is the intended price of the old receiver's 3 px return margin and is
acceptable for a hand-off; it replaces the previous hole where a large burst
was forwarded and caused `LeaveAt` churn. Any further tuning (shorter window,
forward-motion-gated release) needs a real Windows run - out of scope here.

### 8.2 Real Windows backend geometry

The installed old Windows build uses `input-emulation/src/windows.rs` +
`absolute.rs` at `5233b05` (`EDGE_RETURN_MIN_OVERSHOOT = 2`, 1 px `EnterAt`
inset) rather than the libei tracker modelled here. Both are covered by the
pass claim for the reasons in §2, but only a real Windows 0.2.18 run proves the
end-to-end feeling. The manual matrix is in §9.

### 8.3 Model-based harness

The harness does not execute `capture::CaptureTask` (private binary module); it
models the guard and receiver as pure functions. Drift is bounded by the
model-parity test (§4.3), the behavioural production unit tests, and the
independent task-6 verification. The trace-provenance test is local-only: it
skips when no `handoff-trace.jsonl` exists (CI, fresh machine) and fails if the
local trace has rotated the cited records away - in that case re-ground the
corpus and update §3.

### 8.4 Delivery/coalescing is not simulated byte-for-byte

The guard runs **before** the coalescer in production, so its per-event
decisions are exactly the model; the coalescer only merges forwarded deltas
(their sum is preserved, which is what the old tracker consumes). The
`coalescer_flushes_the_first_forward_of_a_handoff_immediately` test pins the
"forward is never delayed" half against the real `src/coalesce.rs`.

## 9. Manual matrix for the old Windows receiver

Minimum real-device check for the old Windows 0.2.18 build (source = current
Ubuntu tree with the final window-only guard):

1. Same LAN, single 1920x1080 Windows monitor, pointer enters through Windows'
   left edge (Windows on the right of Ubuntu).
2. With `LAN_MOUSE_HANDOFF_TRACE=1` on Ubuntu, cross slowly (touchpad) and fast
   (mouse flick) 10x each, including very fast flicks. Expect: no Windows
   `LeaveAt` within 200 ms of the `Enter`; the Ubuntu cursor disappears
   immediately; no second `Enter` right after the first.
3. Check the trace: `capture.reverse_motion_dropped` lines carry `dx`, `dy`,
   `remaining_px` (saturating at `0` for big bursts), `elapsed_us`,
   `guard_remaining_us`; a normal crossing has `remaining_px` well above `0`,
   and a large warp logs `remaining_px = 0` while the guard stays armed to the
   window deadline.
4. Deliberate hand-back: after the pointer settled on Windows for > 250 ms,
   push back over the same edge. Expect control to return with normal effort
   (`a_reverse_return_after_the_guard_window_is_forwarded`). A hand-back begun
   within the first 200 ms may take up to 200 ms to take effect - intended
   (§8.1).
5. Regression watch: if a `LeaveAt` still appears right after entry, capture
   the trace; with the window-only guard there is no size-based leak left, so
   the remaining candidates are the real Windows backend geometry (§8.2) or a
   non-reverse mechanism (e.g. a stale `LeaveAt` retry), not the source residual
   this harness covers.

## Appendix A - model formulas

- Source side, peer screen on the right (`Position::Right`):
  forward = `dx > 0`, reverse/outward = `-dx > 0`.
- Receiver entered through its left edge: outward position
  `o = -sum(forwarded dx)`; return when `o > RETURN_MARGIN_PX (3.0)`.
- Final window-only guard, per captured motion with virtual time `t` after
  `Begin`:
  - if no guard armed: forward;
  - if `t >= begin + 200 ms`: clear guard, forward;
  - else if outward <= 0: forward (guard/telemetry unchanged);
  - else: drop the whole delta, however large; `remaining = max(0, remaining -
    outward)` is trace telemetry only and never ends the guard.
- The window deadline is fixed at `Begin` and is only cleared lazily by the
  next pointer event; dropped deltas never extend it. Reverse motion after the
  window is forwarded unchanged.

## Appendix B - updating the corpus

The trace rotates at 5 MiB with one backup. When the cited records rotate out,
`corpus_is_grounded_in_the_local_handoff_trace` fails and lists the missing
`stage/serial/dx/dy` tuples. Re-read a fresh trace (same reproduction as
`docs/testing/handoff-trace.md`), replace the `TraceBasis` entries in
`tests/qa_handoff_old_receiver.rs`, keep the per-scene deltas consistent with the
new aggregates, and update the tables in §3 and §5.
