# Hand-off to the old Windows receiver: independent verification (task-6)

Independent QA of the follow-up hand-off fix (task-4 "guard post-forward
reverse hand-off deltas" plus its task-7 follow-up "cover large reverse
activation bursts") against the **old Windows 0.2.18 receiver**
(`v0.2.18` = `5233b05`, `RETURN_MARGIN_PX = 3.0`, exact `EnterAt` edge
anchor), plus the interaction between the source-side cursor hide and the fix.
The final guard is **window-only**: inside the fixed 200 ms post-`Begin`
window every reverse delta is dropped regardless of size; the 32 px constant
survives only as trace telemetry.

User symptom this closes: control moves from the Ubuntu source to the Windows
host, the Windows cursor starts moving, and then Windows hands control back
(`LeaveAt`) because the source forwarded the touchpad backswing that arrives
*after* the first forward delta (release 0.2.18 receiver returns on any
outward push beyond its edge margin). The result was an enter/leave churn that
also prevented the source cursor hide from settling.

**No production file was edited by this verification.** The verifier owns
`tests/qa_handoff_old_receiver_verify.rs`,
`scripts/qa-handoff-old-receiver.sh` and this document only.

## Verdict

**PASS** on the immutable tree below (production `src/capture.rs` sha256
`a1a3909f…`, task-7 window-only guard). The old Windows 0.2.18 receiver gets
zero `LeaveAt` triggers for the observed delta scenes (including the serial-2
34.9 px burst and the serial-3 single `-63.272 px` warp), the first forward
motion is forwarded immediately and unchanged (no extra force), the source
cursor hide survives a stable capture and the leave grace and is fully removed
on shutdown, and every required gate is green. Residual risks and the manual
Windows/Ubuntu matrix are in §7.

| headline gate | result |
|---|---|
| independent real-source replay (`tests/qa_handoff_old_receiver_verify.rs`) | **PASS** — 73/73 |
| pre-fix sensitivity (pinned `8b6981d` tree) | **PASS** — pre-fix forwards the `-17 px` backswing and the old receiver returns; first forward was already immediate |
| old Windows receiver returns on replayed trace scenes | **0** (fixed tree); 1+ on the pre-fix tree |
| cursor hide / park shutdown contract | **PASS** — 36/36 shutdown contract + daemon black-box 28/28 |
| `cargo fmt --check` / clippy / Windows cross-check | **PASS** (clippy `-D warnings` clean) |
| `cargo test --workspace --all-features` | **PASS** — 859 passed, 0 failed, 1 ignored (pre-existing X11 display test) |

## 1. Scope, tree and environment

| item | value |
|---|---|
| repository | `/home/gabriel/code/lan-mouse-custom` |
| branch / HEAD | `main` @ `8b6981d` (local-only branch, nothing pushed) |
| production change under test | `src/capture.rs` working-tree change only (task-4 + task-7), pinned below |
| verifier files | `tests/qa_handoff_old_receiver_verify.rs`, `scripts/qa-handoff-old-receiver.sh`, this document |
| old receiver under test | Windows `v0.2.18` = `5233b05` (local tag) |
| host | Ubuntu 26.04, Linux `7.0.0-43-generic`, x86_64, `rustc 1.99.0` (2026-09-28), `cargo 1.99.0`, zig `0.15.2` |
| Windows cross target | `x86_64-pc-windows-gnu` + repo-vendored zig 0.15.2 compiler/archiver/linker wrapper |
| date | 2026-11-13 |
| evidence artifacts | `target/qa-old-receiver/` (git-ignored): `final-tree-before.txt`, `final-gate.log`, per-gate logs under `logs/` |

Production tree (immutable before **and** after the gate run — hashes are
identical at both points):

```
a1a3909f35078511532a17744c09070298d21e60fa930045fd94a19248ede640  src/capture.rs
bd8627a4005702525e58b05505578d0d3d4ab3fd                          # git hash-object src/capture.rs
e820c998909b157d075bbacbe6a094d881575055638069d1624936139ec80d00  # sha256 of `git diff src/capture.rs`
8b6981d22c2d2ef728501c590eba75d15dec6304                          # HEAD
```

Verifier/peer harness files in the verified tree:

```
3fb3efbb2274e7653a9ec4e2f4ac139669918dc8725da697852b8e106437e953  tests/qa_handoff_old_receiver_verify.rs
95a78c5ac13a9760328ae431b20b793ce2bf4a3eeb3090a1b12557f76ca3ca90  tests/qa_handoff_old_receiver.rs   (task-5 + task-8)
ff40664ccd5b5aa6ae6e7d16bada6b6d787d81ec0baaf7008ac38a133a716ddd  scripts/qa-handoff-old-receiver.sh
```

task-5's independent harness (`tests/qa_handoff_old_receiver.rs`,
`docs/testing/handoff-old-receiver-harness.md`) is present in the verified
tree. task-8 reconciled that harness with the final window-only guard (its
earlier model still encoded the superseded 32 px budget decision); this
verification does **not** depend on it for the verdict — the two harnesses
were written independently and agree (see §5).

## 2. What changed (task-4 + final task-7 follow-up)

| item | value |
|---|---|
| file | `src/capture.rs` only |
| root cause | the previous filter dropped reverse deltas only until the **first forward delta**, then disarmed. A v0.2.18 receiver parks on the entry edge and returns control as soon as the tracked pointer is pushed past `edge + 3 px`; the touchpad backswing can arrive *after* the first forward delta, so those residuals leaked and produced `LeaveAt` → release → re-enter churn. An intermediate version bounded the suppression by a **32 px accumulated reverse budget**, which still forwarded large observed activation warps whole (serial 2: `-17.961` + `-16.938` = 34.9 px; serial 3: a single `-63.272 px` delta) and let the old receiver return. |
| fix (final) | a fixed **200 ms** post-`Begin` reverse-only guard (`HANDOFF_REVERSE_GUARD`) that stays armed across forward deltas and drops **every** reverse delta inside the window, regardless of magnitude. Forward deltas are always forwarded immediately with no timed settle and never disarm the guard. After the window, reverse motion is forwarded untouched, so an intentional hand-back is delayed by at most 200 ms but never lost. `HANDOFF_RESIDUAL_MAX_PX = 32.0` remains only as saturating trace telemetry (`remaining_px` / `guard_remaining_us`), not as a decision bound. |
| constants | `HANDOFF_REVERSE_GUARD = 200 ms` (decision bound, named); `HANDOFF_RESIDUAL_MAX_PX = 32.0` (trace telemetry only); both pinned by sentinel tests |
| protocol | unchanged: no edits to `lan-mouse-proto` / `lan-mouse-ipc` / `input-event`; old peers keep working through the negotiated capability path |

Semantics verified:

1. first forward delta after `Begin` is forwarded immediately, unchanged and
   with no extra force (no settle window, no rescale);
2. reverse residuals before **and after** the first forward delta are dropped
   for the fixed 200 ms window, including a single `-63.272 px` warp and the
   `-17.961`/`-16.938` serial-2 burst (34.9 px > the old budget);
3. after the 200 ms window, reverse motion is forwarded (normal return,
   delayed by at most the window and never lost);
4. forward motion is never dropped, delayed or disarmed by the guard
   (direction-aware filter, not a timed all-motion settle);
5. the old receiver is fed only inward motion during the crossing, so it
   never requests `LeaveAt`.

## 3. Old receiver model (v0.2.18 / `5233b05`)

The model in `tests/qa_handoff_old_receiver_verify.rs` is written from the
v0.2.18 source (`input-emulation/src/libei.rs`), independently of the current
production code:

- `EnterAt` anchor: the exact entry-edge pixel (`Left` → `x0`,
  `Right` → `x1 - 1`, `Top` → `y0`, `Bottom` → `y1 - 1`);
- edge band: the pointer must be within `2 px` of the entry edge for a return
  to be considered (v0.2.18 `near_edge`);
- return threshold: `RETURN_MARGIN_PX = 3.0` past the entry edge on the raw
  tracked position (`Left`: `raw_x < x0 - 3`; `Right`: `raw_x > x1 + 3`;
  analogous on Y);
- the tracked position mirrors the compositor clamp into the region, so
  repeated small outward pushes restart from the edge and do not accumulate;
- after firing, the receiver re-arms only when the pointer is `20 px` inside
  the entry screen area.

The Windows runtime path of release 0.2.18 (`input-emulation/src/windows.rs` +
`absolute.rs` @ `5233b05`) is equivalent for this verdict: the `EnterAt` entry
anchor is `1 px` inside the edge (`entry_anchor_fraction_in_monitors(..., 1)`)
and the return fires when the unclamped target overshoots the entry edge by
`>= EDGE_RETURN_MIN_OVERSHOOT = 2 px`, i.e. the same `~3 px` outward push as
the libei tracker above. The verdict does not depend on the exact threshold:
inside the guard the source forwards **only inward deltas**, so neither
detector can observe an outward push at all.

The same model fed the **pre-fix** forwarded trace is the test's sensitivity
check: it must fire. The harness also models the pre-fix source filter to
produce that trace, so a regression in either half is caught.

## 4. How the shipping source is exercised (no production hooks)

`tests/qa_handoff_old_receiver_verify.rs` wraps

```rust
mod capture { include!("../src/capture.rs"); /* verifier replay driver */ }
```

with minimal stubs for the unrelated `client` / `config` / `connect` modules.
The verifier driver is appended **inside that module**, so the real
`CaptureTask::handle_capture_event` (the exact file that ships) is driven with
the trace-shaped delta sequence and its forwarded `ProtoEvent`s are fed to the
old-receiver model. No test hook, feature flag or production edit is needed;
`cfg(test)` unit tests embedded in the compiled file also run and must stay
green.

## 5. Evidence

All results below are from **one** run of
`scripts/qa-handoff-old-receiver.sh all` at `2026-11-13T15:51:27-03:00` →
`15:53:09-03:00` on the immutable tree above (hashes identical before and
after; the script records them in `target/qa-old-receiver/tree.txt` and
`final-tree-before.txt`). Script summary: **13/13 checks passed, 0 failed,
0 skipped**; raw logs in `target/qa-old-receiver/logs/`.

| gate | command | result |
|---|---|---|
| independent real-source replay | `cargo test --test qa_handoff_old_receiver_verify` | **PASS** — 73/73 (includes the shipping file's own `cfg(test)` modules) |
| pre-fix sensitivity (must fail) | same harness compiled against `git archive 8b6981d`, pinned tests | **PASS** — pre-fix forwards `[(3.0,0.0),(-17.0,0.0),(4.0,0.0)]` and the old receiver returns; the first-forward test passes on both trees |
| task-5 harness (reconciled by task-8) | `cargo test --test qa_handoff_old_receiver` | **PASS** — 31/31 |
| formatting | `cargo fmt --check` | **PASS** — no diff |
| guard unit tests (shipping lib) | `cargo test -p lan-mouse --lib` | **PASS** — 280 passed |
| shutdown lifecycle contract | `cargo test --test qa_handoff_shutdown` | **PASS** — 36/36 |
| clipboard regression | `cargo test --test qa_clipboard_atomicity` | **PASS** — 32/32 |
| connect/disconnect regression | `cargo test --test qa_connection_state` | **PASS** — 7/7 |
| whole workspace | `cargo test --workspace --all-features` | **PASS** — 859 passed, 0 failed, 1 ignored (`input-emulation` `x11::tests::connects_to_the_x_server`, requires a display; pre-existing) |
| lints | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | **PASS** — clean |
| Windows cross-check | `cargo check --workspace --all-targets --exclude lan-mouse-gtk --target x86_64-pc-windows-gnu` (repo-vendored zig 0.15.2 as `cc`/`ar`/linker) | **PASS** — `Finished dev profile` |
| daemon shutdown black-box | `scripts/qa-handoff-shutdown.sh` | **PASS** — 28/28 checks |
| full QA harness | `scripts/qa-handoff-old-receiver.sh all` | **PASS** — 13/13 |

### 5.1 Behavioural evidence (fixed tree vs baselines)

| behaviour | fixed tree (`a1a3909f…`) | baseline |
|---|---|---|
| first forward delta after `Begin` | forwarded in the same handler call, exact value `+2.0`, no ack/delay | pre-fix tree: also immediate (**no regression**) |
| post-forward backswing `-17 px` | **not forwarded**; old receiver sees only `[(3,0),(4,0)]`, 0 returns | pre-fix tree: forwarded `[(3,0),(-17,0),(4,0)]`, old receiver returns 1 (reported churn) |
| serial-2 scene `-17.961, +12.390, -16.938` (34.9 px reverse) | reverse deltas suppressed; forwarded = `[(12.390,0)]`; 0 returns | **task-4 intermediate tree** (`1e9e6db1…`): forwarded `[(12.39,0),(-16.938,0)]` and the old receiver would return — the residual hole that task-7 closed |
| serial-3 scene single `-63.272 px` warp | suppressed inside the window; forward delta forwarded; 0 returns | task-4 intermediate tree: forwarded whole as "deliberate hand-back" |
| genuine return after the window | forwarded unchanged, old receiver returns 1 | pre-fix tree: also forwarded (no regression) |
| every entry edge (reverse direction included) | same trace on `Left`/`Right`/`Top`/`Bottom` → only inward deltas forwarded, 0 returns | — |
| timed all-motion settle | `ENTER_MOTION_SETTLE` absent; sentinel test + first-forward timing test pass | — |

Historical note: task-5's harness found the >32 px burst hole (its §8.1);
task-7 changed the guard to window-only and this verifier's
`large_reverse_bursts_inside_the_guard_never_reach_the_old_receiver`
acceptance test was red on the task-4 tree and green on the final tree. That
makes the final fix independently confirmed from both directions: an
incremental verifier caught the hole, the fix closed it, and the replay is now
green. task-8 reconciled task-5's pure model with the window-only guard before
this final gate (task-5 harness file hash in §1).

## 6. Regression audit

Checks performed beyond the replay:

- **cursor hide while the source capture is stable**: heartbeat file exists
  and is refreshed; it survives the post-leave grace (no border flash) and is
  removed afterwards; `clear()`/`shutdown()`/`drop` remove both
  `cursor-hide` and `cursor-park`;
- **shutdown**: the existing black-box daemon scenarios
  (`scripts/qa-handoff-shutdown.sh`: SIGTERM, `cli shutdown`, stale markers
  from a crashed instance) must stay green — the fix does not touch the
  shutdown path, but the acceptance criterion is that closing Lan Mouse still
  removes both markers;
- **clipboard**: `tests/qa_clipboard_atomicity.rs` and the clipboard crates
  are untouched by the diff and run in the workspace suite;
- **park / reverse direction**: parking must never shortcut a live outgoing
  capture; the same post-forward backswing is verified for all four entry
  edges, which includes the reverse direction (Windows source → Ubuntu
  receiver);
- **protocol compatibility**: the diff is confined to `src/capture.rs`; no
  wire/layout change, so a 0.2.18 peer keeps negotiating the same
  capabilities (`ENTER_ACK`, `SERIALIZED_LEAVE`, `EDGE_COORDINATE`).

Audit results:

| audit item | evidence | result |
|---|---|---|
| production diff scope | `git diff --name-only` → `src/capture.rs` only; `git diff --name-only -- lan-mouse-proto lan-mouse-ipc input-event input-emulation input-capture` → empty | **PASS** |
| cursor hide during a stable source capture | harness test `a_stable_capture_keeps_the_hide_and_shutdown_removes_both_markers` + compiled `src/cursor_hide.rs` tests (`active_capture_creates_and_removes_the_heartbeat`, `a_leave_keeps_the_cursor_hidden_during_the_grace_period`, …) | **PASS** — heartbeat present and refreshed, grace preserved |
| shutdown removes `cursor-hide` **and** `cursor-park` | `CursorHide::shutdown()` removes both (harness); `src/cursor_hide.rs::shutdown_removes_markers_written_by_another_instance`, `drop_removes_the_parked_cursor_marker_too`; `tests/qa_handoff_shutdown` 36/36; daemon scenarios SIGTERM / `cli shutdown` / stale-crash markers 28/28 | **PASS** |
| park never shortcuts a live capture | `parking_is_refused_while_a_capture_owns_the_cursor`, harness park assertion (`park()` is a no-op while `owner.is_some()`); `clear()`/`shutdown()` remove the park marker | **PASS** |
| clipboard regressions | `tests/qa_clipboard_atomicity` 32/32, workspace suite includes `input-clipboard` (0 failures); diff does not touch clipboard files | **PASS** |
| reverse-direction hand-off (Windows → Ubuntu) | harness replays the same post-forward backswing for all four capture positions/entry edges | **PASS** |
| no timed all-motion settle | `ENTER_MOTION_SETTLE` count in `src/capture.rs` = 0; sentinel test `the_handoff_guard_is_bounded_and_reverse_only`; first-forward test | **PASS** |
| tree immutability during the gate | all source/test/doc hashes identical in `final-tree-before.txt` and after the run (`capture.rs` `a1a3909f…`, task-5 test `95a78c5a…`, this harness `3fb3efbb…`) | **PASS** |

Operational note found during verification: `scripts/qa-handoff-shutdown.sh`
requires an **absolute** `QA_BASE_DIR`. With a relative base directory the
daemon treats `XDG_CONFIG_HOME` as invalid (XDG requires an absolute path),
ignores the test config and binds the default `4242`/`4245` ports, which are
already taken on this host — the scenarios then fail spuriously. The new
`scripts/qa-handoff-old-receiver.sh` always passes absolute paths.

## 7. Residual risks and manual matrix

Automated verification cannot replace the real OS path (Mutter/libei capture,
touchpad hardware, real Windows hook). Everything below the manual matrix is
deterministic on this host; the rows below need the two physical machines.

Residual risks (accepted / documented, not blockers):

1. **Intentional immediate hand-back is delayed by up to 200 ms.** A reverse
   gesture started inside the post-`Begin` window is suppressed until the
   window ends; it is never lost. This is the deliberate trade-off that makes
   the crossing feel like a physical monitor edge (no backswing churn). T3/T4
   below check the real feel.
2. **No real Windows 0.2.18 run on this host.** The old-receiver model is
   traced to `5233b05` (libei tracker `RETURN_MARGIN_PX = 3.0`; Windows
   backend `EDGE_RETURN_MIN_OVERSHOOT = 2` with a 1 px `EnterAt` inset) and the
   fixed guard forwards only inward deltas inside the window, which no
   entry-edge return detector can trigger. Real datapath (DTLS coalescing
   order, Wi-Fi reordering, Windows `SendInput` clamping) still needs T1/T2/T6.
3. **Delivery/coalescing is not simulated byte-for-byte.** The guard runs
   before the coalescer, so its per-event decisions are exactly what the replay
   sees; the coalescer only merges already-forwarded deltas (their sum is
   preserved, which is what the older receiver consumes).
4. **Trace telemetry is not a budget any more.** `remaining_px`/`guard_remaining_us`
   on `capture.reverse_motion_dropped` are diagnostic; `remaining_px`
   saturates at `0` and must not be read as "suppression still available".
   Analysis scripts should use the 200 ms window, not the 32 px value.
5. **task-5's pure-function model was reconciled by task-8** to the window-only
   guard before the final gate; this document's verdict rests on the real
   shipping-source replay (§4/§5), which is authoritative.

| # | scenario | expected | how |
|---|---|---|---|
| T1 | Ubuntu touchpad → Windows 0.2.18, 10 crossings, keep moving the finger after crossing | Windows cursor starts immediately (< ~50 ms), no hesitation, no jump back; Ubuntu cursor stays hidden for the whole session; no repeated `Enter`/`LeaveAt` | Windows console + `LAN_MOUSE_HANDOFF_TRACE=1` on both sides; `scripts/analyze-handoff-trace.py` shows one controller session per crossing, no `receiver.first_input` churn |
| T2 | same crossing, backswing-heavy touchpad gesture (fast flick then settle) | no enter/leave ping-pong; at most one `Enter` per crossing | trace: no `controller.reverse_motion_dropped` followed by `LeaveAt` within the guard window |
| T3 | return Windows → Ubuntu after the guard window | hand-back works with no extra force; Ubuntu cursor reappears at the return coordinate | trace: reverse deltas forwarded after 200 ms; `LeaveAt`/`Leave` accepted |
| T4 | return within 200 ms of entry (deliberate quick hand-back) | the reverse push is delivered once the 200 ms window ends (≤ 200 ms delay, no extra force beyond the delay) | manual; expect a short pause for a deliberate immediate return, never a swallowed gesture |
| T5 | close Lan Mouse on Ubuntu while a Windows session is active | cursor-hide and cursor-park removed, Ubuntu cursor visible at the edge/top | `scripts/qa-handoff-shutdown.sh` covers the daemon side; manual for the GUI close |
| T6 | Windows 0.2.18 receives a forced return push > 3 px | one `LeaveAt`, no loop | manual; confirms the old receiver compatibility contract in production |

## 8. Update after task-10 / 0.2.19 (bidirectional guard)

The old-receiver conclusions above still hold on the final 0.2.19 tree
(`ac728e5`), now with the task-10 **position-aware** extension of the same
`src/capture.rs` guard (`docs/testing/handoff-bidirectional-verification.md`):

- the filter still drops **all** reverse deltas while the receiver cursor is
  next to the entry edge (displacement below `HANDOFF_ENTRY_BAND_PX = 16 px`),
  independent of the 200 ms window, and forwards **all** forward deltas
  immediately;
- a guarded source therefore still feeds the old Windows 0.2.18 receiver only
  inward motion, so it produces **0** `LeaveAt` triggers for the same trace
  scenes (re-verified by `tests/qa_handoff_bidirectional.rs` and by the
  adapted `tests/qa_handoff_old_receiver_verify.rs`);
- the old receiver harness's "return after the window" helper now expires both
  deadlines (the fixed 200 ms telemetry window and the 250 ms position-aware
  bound): under the final guard a reverse gesture is only "genuine" after the
  pointer moved clearly inward or the bounded filter expired, which matches
  the task-10 contract and the analysis of the 204.0–205.4 ms settling cluster.

No production behaviour regressed: the final gate in this document's sibling
verification (`handoff-bidirectional-verification.md` §5) includes this
harness (79/79) and the previous full gate script.
