# Bidirectional hand-off verification (task-11)

Independent verification of the two-way hand-off churn reported while the
Ubuntu machine runs the current `main` build and the Windows machine runs the
old **0.2.18** release (`5233b05`), and of the **same-build 0.2.19 candidate**
(no publish; local validation only).

Reported symptoms (both directions on the same desk):

1. **Ubuntu source → Windows 0.2.18 receiver**: Ubuntu shows the capture
   entering the client and then receiving `LeaveAt` / leaving again after
   roughly 2 s, repeating; the Ubuntu cursor stays visible at the border.
2. **Windows 0.2.18 source → Ubuntu receiver**: the Ubuntu receiver logs
   repeated `handoff: return request ... along=None` (plus
   `pushed past the Right edge; handing control back`) while Windows is still
   controlling, cycling with `releasing capture: ... entered this device`.

**No production file is edited by this verification.** Deliverables owned by
the verifier: `tests/qa_handoff_bidirectional.rs`,
`scripts/qa-handoff-bidirectional.sh`, this document.

## Verdict

**PASS** on the frozen same-build **0.2.19** candidate (`main` @ `ac728e5`,
production `src/capture.rs` sha256 `4f3dd321…`, `input-emulation/src/libei.rs`
sha256 `9135f6ea…`). The position-aware source filter and the receiver-side
re-entry gate fix the two causes ranked by task-9: a late reverse residual next
to the entry edge is suppressed in both directions, reverse motion after the
pointer clearly moved inward is forwarded immediately (no extra force), the
`along=None` EnterOnly re-entry loop cannot repeat, the receiver deadband
ignores immediate entrance residual while a deliberate return still works, and
all candidate/repo/previous-regression gates are green (17/17 checks, plus the
nested previous full gate 13/13). The verified tree is immutable: all hashes
before and after the gate run are identical (§5.1). Residual risks and the
same-build manual matrix are in §7/§8.

| headline gate | result |
|---|---|
| independent bidirectional replay (`tests/qa_handoff_bidirectional.rs`) | **PASS** — 85/85 |
| same-build 0.2.19 candidate (version, Ubuntu release build, Windows cross, packaging) | **PASS** |
| `cargo test --workspace --all-features` | **PASS** — 961 passed, 0 failed |
| `cargo clippy … -D warnings` / `cargo fmt --check` | **PASS** — clean |
| previous hand-off/shutdown/clipboard gates | **PASS** — 79/79, 31/31, 36/36, 32/32, 7/7; daemon QA 28/28; previous full gate 13/13 |
| `scripts/qa-handoff-bidirectional.sh all` | **PASS** — 17/17 checks, 0 failed, 0 skipped |

## 1. Scope and environment

| item | value |
|---|---|
| repository | `/home/gabriel/code/lan-mouse-custom` |
| branch / HEAD (verified) | `main` @ `ac728e5` — "fix(handoff): position-aware bidirectional guard" (0.2.19; local-only, nothing pushed) |
| Ubuntu side (reported) | `a3e1ad8` (`0.2.18`, before the bidirectional fix) |
| Windows side (reported) | release `0.2.18` = `5233b05` (old receiver/source) |
| candidate under test | same-build **0.2.19** = `ac728e5` on both sides (no publish; release-owner commit by the Lead) |
| production changes | `src/capture.rs`, `input-emulation/src/libei.rs` only (task-10); protocol files untouched |
| host | Ubuntu, Linux `7.0.0-43-generic`, x86_64, `rustc 1.99.0`, `cargo 1.99.0`, zig `0.15.2` |
| Windows cross target | `x86_64-pc-windows-gnu` + repo-vendored zig 0.15.2 wrappers |
| date | 2026-11-13 |
| gate window | `17:35:46` → `17:38:29` (America/Sao_Paulo) |

| artifact | sha256 |
|---|---|
| `src/capture.rs` | `4f3dd3210aabdab18f4785103bd3ef8ae49514883aa9f9203932cdf15f1b2e26` (`git hash-object` `62a81bfe…`) |
| `input-emulation/src/libei.rs` | `9135f6ea1394801549021267c50ef875c2ff505e2d706f43e9e18e6feb07d571` |
| `tests/qa_handoff_bidirectional.rs` | `8cea93b3afdad69461db0fa856c70ca770beafa099f6e1993ca2e0435de23f62` |
| `tests/qa_handoff_old_receiver_verify.rs` (adapted) | `dcfc63356071e5cee5fb5af326fd1c09dd1526e5f3fbc60c98ccc8bce5a80115` |
| `scripts/qa-handoff-bidirectional.sh` | `1354fb466af419d28c25968d2a6f7c84d5e872a1d7cfdca45beff61fc4323fc1` |
| `scripts/qa-handoff-old-receiver.sh` (adaptation fix) | `d9571f68994eccb30ab145b94a851c066a4338100d85e8009b91789c327840e3` |

Doc files are finalized after the gate; code/test/script hashes above are the
ones the gate ran with. No production file was edited by this verification.

Working-tree state and the exact `src/capture.rs` /
`input-emulation/src/libei.rs` hashes are recorded in §5 once the
verification tree is frozen (task-10 decides whether production changes are
needed; task-9 may conclude the same-build upgrade is sufficient).

## 2. Evidence basis (audited before the fix)

The reported behaviour is corroborated by the local journal and hand-off trace
files. Representative lines (Ubuntu receiver, old Windows 0.2.18 source):

```
INFO  input_emulation::libei] pushed past the Right edge; handing control back
INFO  lan_mouse::service] handing control back: remote pointer left through Right (192.168.166.166:56675)
INFO  lan_mouse::service] handoff: return request to 192.168.166.166:56675: supported=true along=Some(0.46) caps=Some(1016)
INFO  lan_mouse::emulation] releasing capture: 192.168.166.166:56675 entered this device
```

repeating roughly once per second, and the `along=None` variant reported for
the 16:15 session. `handoff: return request ... along=None` can only be
produced when the receiver had no crossing coordinate to carry back
(legacy `Enter` from the old peer) — see `src/service.rs::request_handoff`.

The same-build hypothesis under test: the source-side reverse-residual guard
and the receiver-side hand-off fixes exist only in the current build
(`a3e1ad8`), so with a 0.2.18 peer one direction always runs the old,
unguarded source (or old receiver) logic. Both directions should be quiet when
both machines run the same candidate build.

task-9's full analysis is in `docs/analysis/handoff-bidirectional.md`; the
facts this verification rests on are:

| # | fact | evidence |
|---|---|---|
| F1 | The Windows receiver code is **identical** in `v0.2.18` and `main`: `input-emulation/src/windows.rs`, `absolute.rs`, `input-capture/src/windows/*` and `lan-mouse-proto` are unchanged. A same-build upgrade does not change the endpoint that emits `LeaveAt`. | `git diff --stat v0.2.18..main -- <those paths>` = empty |
| F2 | The Ubuntu source guard in `a3e1ad8` is **time-only** (`HANDOFF_REVERSE_GUARD = 200 ms`); trace samples show the settling boundary at **204.0–205.4 ms** in 30/95 first-motion samples, so late reverse residuals are forwarded. | `src/capture.rs`:390; `handoff-trace.jsonl{,.1}` `capture.first_motion_sent` |
| F3 | The reported Ubuntu→Windows churn ran **on `a3e1ad8`** (the final guard): `LeaveAt(83..92)` at 16:15:35–16:15:53, `Enter` acked immediately each time → not `ENTER_TIMEOUT`, not protocol mismatch. | journal `195582`, `docs/analysis/handoff-bidirectional.md` §4.1 |
| F4 | The `along=None` storm is a **receiver-side re-entry loop**, not an edge return: `ICaptureEvent::CaptureBegin` on the incoming `EnterOnly` capture while `remote_peer.is_some()` → `request_handoff(addr, None)` (`Leave`); the peer releases, local capture re-enables with the injected cursor parked on the edge, and the next `Begin` fires again (7–10/s at 16:15:33). | `src/service.rs:1291-1303,1434-1489`; `src/capture.rs::is_suppressed_remote_begin`; journal 16:15:33 |
| F5 | The old `0.2.18` **source** has no reverse guard at all, so mixed-build Windows→Ubuntu feeds the now 1-pixel-sensitive Ubuntu receiver unguarded activation residuals (1 Hz `along=Some` series). | `git show v0.2.18:src/capture.rs`; `input-emulation/src/libei.rs`:148,255; journal 15:40:52–15:41:12 |

Consequence: the candidate must contain **both** a position-aware source filter
and a receiver-side re-entry gate; a same-build upgrade alone is not expected
to fix either direction (F1–F4).

## 3. Fixes under test

task-10 lands two independent production changes, both verified here:

**A. Source position-aware leading-residual filter (`src/capture.rs`).**
`CaptureTask` now tracks `handoff_inward_px` — the inward displacement of the
receiver cursor from the entry edge, integrated from the deltas the source
actually forwarded. Reverse deltas are dropped while that estimate is below
`HANDOFF_ENTRY_BAND_PX = 16.0` logical px (wider than the largest observed
leading forward warp, 12.39 px) **and** either the old fixed
`HANDOFF_REVERSE_GUARD = 200 ms` window or the bounded
`HANDOFF_POSITION_GUARD = 250 ms` filter is still armed (the 250 ms bound
covers the observed 204.0–205.4 ms settling cluster). Forward deltas are
always forwarded immediately and integrated into the estimate; once the
pointer has moved clearly inward (or the 250 ms bound expires) the filter
disarms for good, so reverse motion is user intent again. The fixed window
survives only as telemetry/deadline; no timed all-motion settle is involved.

**B. Receiver `along=None` re-entry gate (`src/capture.rs`, no `service.rs`
change).** `handle_capture_event` now emits `ICaptureEvent::CaptureBegin` for
an `EnterOnly` begin only when `should_report_enter_only_begin` allows it:
while `remote_peer.is_some()`, begins during the first
`REMOTE_ENTRY_DWELL = 300 ms` after the peer started driving (`remote_peer_since`,
reset by `update_remote_peer` on a real edge change) are settling jitter, and
after the dwell exactly one deliberate local take-back is surfaced per
remote-driving session (`enter_only_take_back_sent`; the service retries the
request itself). The old parked-pointer loop `CaptureBegin → request_handoff(addr,
None) → peer releases → re-entry` therefore cannot repeat.

**C. Receiver return deadband (`input-emulation/src/libei.rs`).**
`AbsoluteTracker` gained `armed_at`/`inward_travel_px` and a `return_ready()`
gate: `near_edge && past_edge` only fires when the pointer has travelled at
least `ENTRY_WORK_DEADBAND_PX = 2.0` px inward since arming and
`ENTRY_RETURN_SETTLE = 250 ms` have elapsed. Immediate entrance residual /
edge jitter is ignored, while a deliberate reverse after real use of the
screen still hands control back immediately.

The full task-10 verification targets, for reference:

1. **source side** (`src/capture.rs`): replace/augment the fixed 200 ms
   reverse-only window with a **position-aware** leading-residual filter —
   reverse deltas are dropped while the tracked remote pointer is still inside
   the entry band (roughly ≤ 8–12 logical px of the crossing edge), regardless
   of elapsed time; forward deltas are forwarded immediately and never
   delayed; once the pointer has moved clearly inward, reverse motion passes
   (deliberate return, no extra force). The 200 ms window survives at most as
   a hard upper bound/telemetry, not as the sole decision.
2. **receiver side**: gate spurious `CaptureBegin` on the incoming
   `EnterOnly` capture while `remote_peer.is_some()` (and/or a small
   positional deadband before an edge return), so the parked injected pointer
   cannot re-trigger `request_handoff(addr, None)` in a loop; a deliberate
   local grab must still work.

_See above; exact hashes and results in §5._

## 4. Deterministic verification method

`tests/qa_handoff_bidirectional.rs` compiles the shipping `src/capture.rs`
read-only into the test crate (`mod capture { include!(...) }`, tiny stubs for
the unrelated `client`/`config`/`connect` modules) and drives the private
`CaptureTask::handle_capture_event` with trace-shaped delta sequences for both
directions. The deltas the real task forwards feed two independently written
receiver models:

- `NewUbuntuReceiver` — current libei `AbsoluteTracker`: `RETURN_MARGIN_PX =
  0.0`, `EnterAt` anchor on the edge / legacy `Enter` anchor inset 2 px,
  outward-direction check, 20 px re-arm, task-10 `ENTRY_WORK_DEADBAND_PX` /
  `ENTRY_RETURN_SETTLE` gate;
- `OldWindowsReceiver0218` — release 0.2.18 Windows backend tracker: `EnterAt`
  anchor 1 px inside the edge, `EDGE_RETURN_MIN_OVERSHOOT = 2` integer
  physical px, entry-edge-only return.

`capture::a_late_reverse_residual_near_the_entry_edge_is_suppressed` and
`capture::a_reverse_gesture_after_moving_inward_is_forwarded` model the two
source-side acceptance criteria directly; the re-entry loop is driven through
the real handler and observed on the `ICaptureEvent` channel
(`capture::jittery_enter_only_begins_while_remote_drives_do_not_loop`,
`…a_deliberate_take_back_after_the_dwell_is_reported_once_per_session`,
`…an_enter_only_begin_without_a_remote_peer_still_notifies_the_service`).
Sensitivity checks (`an_unguarded_0218_windows_source_…`,
`an_unguarded_source_triggers_the_old_windows_receiver`) prove the mixed-build
churn is reproducible, so a green run is meaningful. Static sentinels pin the
bounded constants in both production files so a future tuning change that
reintroduces a wide/long filter fails loudly. Cursor hide/park/shutdown are
re-checked from the shipped `src/cursor_hide.rs`; the previous verifier
harness `tests/qa_handoff_old_receiver_verify.rs` is adapted to the new
position-aware contract and stays in the regression set.

## 5. Gates and results

All results below are from **one** run of
`scripts/qa-handoff-bidirectional.sh all` at `2026-11-13T17:35:46-03:00` →
`17:38:29-03:00` on the frozen tree (`ac728e5`, version 0.2.19). Script
summary: **17/17 checks passed, 0 failed, 0 skipped**. Raw logs:
`target/qa-bidi/logs/` (git-ignored). Tree fingerprint:
`target/qa-bidi/final-tree-before.txt`.

| gate | command | result |
|---|---|---|
| independent bidirectional replay | `cargo test --test qa_handoff_bidirectional` | **PASS** — 85/85 |
| candidate version metadata | `Cargo.toml` / `lan-mouse-gpui/Cargo.toml` = `0.2.19` | **PASS** |
| Ubuntu local release build | `cargo build -p lan-mouse --release --no-default-features` | **PASS** — `lan-mouse 0.2.19`, `commit_hash:ac728e59` |
| Windows cross-compile | `cargo check --workspace --all-targets --exclude lan-mouse-gtk --target x86_64-pc-windows-gnu` (vendored zig wrappers) | **PASS** — `Finished dev profile` |
| installer/packaging checks | `cargo test --test packaging` | **PASS** — 20/20 |
| formatting | `cargo fmt --check` | **PASS** — no diff |
| guard unit tests | `cargo test -p lan-mouse --lib` | **PASS** — 286/286 |
| receiver deadband/settle tests | `cargo test -p input-emulation --lib` | **PASS** — 83/83 |
| previous verifier gate (adapted) | `cargo test --test qa_handoff_old_receiver_verify` | **PASS** — 79/79 |
| task-5 old-receiver harness | `cargo test --test qa_handoff_old_receiver` | **PASS** — 31/31 |
| shutdown lifecycle contract | `cargo test --test qa_handoff_shutdown` | **PASS** — 36/36 |
| clipboard regression | `cargo test --test qa_clipboard_atomicity` | **PASS** — 32/32 |
| connect/disconnect regression | `cargo test --test qa_connection_state` | **PASS** — 7/7 |
| whole workspace | `cargo test --workspace --all-features` | **PASS** — 961 passed, 0 failed |
| lints | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | **PASS** — clean |
| daemon cursor lifecycle | `scripts/qa-handoff-shutdown.sh` | **PASS** — 28/28 checks |
| previous full hand-off gate | `scripts/qa-handoff-old-receiver.sh all` (nested, `QA_OUT=<short>/p`) | **PASS** — 13/13 checks |
| full bidirectional harness | `scripts/qa-handoff-bidirectional.sh all` | **PASS** — 17/17 |

### 5.1 Tree immutability

Every hash in `final-tree-before.txt` is identical after the run (verified by
re-hashing all gate-relevant files and comparing): production `src/capture.rs`
`4f3dd321…`, `input-emulation/src/libei.rs` `9135f6ea…`, HEAD `ac728e5`,
harness/script hashes as in §1. The only files that changed after the gate are
this document and the two verification docs (text only; they are not compiled
or executed by any gate).

Operational notes from this verification:

- the daemon QA legs bind a Unix socket under the work directory; with this
  repository path a long `QA_OUT` exceeds the 108-byte `SUN_LEN` limit
  (`path must be shorter than SUN_LEN`). The script now uses
  `target/qa-bidi` and a 1-letter subdirectory for the nested previous gate.
- the previous verifier harness was adapted to the final position-aware
  contract (two extra `CaptureTask` fields plus the 250 ms positional bound);
  `scripts/qa-handoff-old-receiver.sh`'s pre-fix sensitivity adaptation was
  updated accordingly so the historical 8b6981d comparison still compiles and
  still fails for the expected -17 px backswing leak.

### 5.2 Behavioural evidence on the final tree

| behaviour | evidence | result |
|---|---|---|
| source late reverse residual (204-205 ms cluster class) while next to the edge | `capture::a_late_reverse_residual_near_the_entry_edge_is_suppressed` (both directions; fixed 200 ms window expired, position bound still armed) | **suppressed** |
| forward motion immediately after the crossing | `capture::the_first_forward_delta_is_forwarded_immediately_in_both_directions` | forwarded same-call, exact value |
| reverse after clearly moving inward (no extra force) | `capture::a_reverse_gesture_after_moving_inward_is_forwarded` | forwarded immediately |
| bounded filter escape (held reverse near the edge) | `capture::the_position_guard_is_bounded_so_a_held_reverse_gesture_passes` | forwarded after the 250 ms bound |
| large activation burst (serial-2 34.9 px reverse) | `capture::large_activation_bursts_are_still_suppressed_near_the_edge` | suppressed, old receiver 0 returns |
| same-build source → current Ubuntu receiver | `capture::a_guarded_windows_source_keeps_the_new_ubuntu_receiver_quiet` | 0 return requests |
| same-build source → old 0.2.18 Windows receiver | `capture::a_guarded_ubuntu_source_keeps_the_old_windows_receiver_quiet` | 0 `LeaveAt` |
| mixed-build old 0.2.18 source (sensitivity) | `capture::an_unguarded_0218_windows_source_triggers_the_new_ubuntu_receiver_after_settling` | reproduces the churn (immediate entrance residual is ignored by the new deadband; a late residual after the settle still fires) |
| mixed-build unguarded source → old receiver (sensitivity) | `capture::an_unguarded_source_triggers_the_old_windows_receiver` | reproduces the `LeaveAt` |
| receiver `along=None` re-entry loop | `capture::jittery_enter_only_begins_while_remote_drives_do_not_loop` | 0 `CaptureBegin` events for repeated jittery begins |
| deliberate local take-back | `capture::a_deliberate_take_back_after_the_dwell_is_reported_once_per_session` | exactly 1 request per remote session; re-armed on a new session |
| gate scope | `capture::an_enter_only_begin_without_a_remote_peer_still_notifies_the_service` | unchanged when no remote peer drives |
| deliberate return after the receiver settle | `capture::a_genuine_return_is_still_delivered_in_both_directions` + `new_receiver::return_after_working_inside`-style model checks | relayed; `LeaveAt`/`Leave` hand-back works |
| constants stay bounded | `the_bidirectional_guard_and_deadband_stay_bounded` (static sentinels) | band ≤ 32 px, position bound ≤ 400 ms, dwell ≤ 1000 ms, deadband ≤ 8 px, settle ≤ 500 ms |

## 6. Regression audit

| audit item | evidence | result |
|---|---|---|
| production diff scope | `git diff --name-only ac728e5~1..ac728e5` → `src/capture.rs`, `input-emulation/src/libei.rs`, version metadata; protocol crates untouched (`git diff --name-only … -- lan-mouse-proto lan-mouse-ipc input-event input-capture` empty) | **PASS** |
| no timed all-motion settle | static sentinel + replay: forward deltas are forwarded immediately even before the Ack | **PASS** |
| cursor hide during a stable source capture | compiled `src/cursor_hide.rs` tests + harness cursor contract (`a_stable_capture_keeps_the_hide_and_shutdown_removes_both_markers`); heartbeat present/refreshed, grace kept | **PASS** |
| shutdown removes `cursor-hide` and `cursor-park` | `CursorHide::shutdown`/`drop` unit tests, `tests/qa_handoff_shutdown` 36/36, daemon black-box 28/28 (SIGTERM / `cli shutdown` / stale crash markers) | **PASS** |
| park never shortcuts a live capture | `src/cursor_hide.rs` parking tests + base behaviour unchanged (no diff in `src/cursor_hide.rs`/`src/service.rs`) | **PASS** |
| clipboard regressions | `tests/qa_clipboard_atomicity` 32/32, workspace suite includes `input-clipboard` (0 failures), no clipboard file in the diff | **PASS** |
| reverse-direction continuity (Windows → Ubuntu) | same bidirectional replay suite covers `Position::Left` source and Right-edge receiver; re-entry gate is receiver-side and direction-symmetric | **PASS** |
| previous hand-off guarantees (old 0.2.18 receiver compatibility) | `qa_handoff_old_receiver_verify` 79/79 + `qa_handoff_old_receiver` 31/31 + previous full gate 13/13 (incl. pre-fix sensitivity against `8b6981d`) | **PASS** |
| tree immutability during the gate | §5.1 | **PASS** |

## 7. Same-build manual matrix (Windows ↔ Ubuntu)

Both machines must run the **same candidate build** (same commit, version
0.2.19 once the Lead bumps it). 20 crossings per direction, alternating, with
`LAN_MOUSE_HANDOFF_TRACE=1` on both sides and `journalctl -f` captured.

| # | scenario | expected | how to count |
|---|---|---|---|
| M1 | Ubuntu → Windows, 20 crossings, keep moving the finger after the crossing | exactly one `capture.begin` / `capture.enter_sent` per crossing on the Ubuntu source; **no** `LeaveAt` / `leaving client` within 5 s of a crossing; Windows cursor moves immediately | Ubuntu journal: `grep -c 'leaving client'` must equal intentional returns only; `scripts/analyze-handoff-trace.py` one controller session per crossing |
| M2 | Windows → Ubuntu, 20 crossings | Ubuntu receiver: no `handoff: return request ...` while Windows is still moving; at most one `receiver.enter_received`/`pushed past` per intentional return | receiver journal: `grep -c 'handoff: return request'` ≈ 0 outside deliberate returns; `along=None` count must stay 0 during normal control |
| M3 | 20 fast in/out flips at the same edge position | no same-second `Enter → Leave → Enter` machine-cadence loops (humans cannot sustain ≥1 Hz flips); no `along=None` burst | journal timestamps: consecutive enter/leave pairs must be ≥ ~1 s apart and follow a deliberate gesture |
| M4 | deliberate return in both directions (move clearly inside, then reverse) | return happens with **no extra force**: no "push through a wall"; at most the receiver's 1-3 px edge overshoot beyond the entry edge | journal: one `LeaveAt`/`Leave` per deliberate return, immediate |
| M5 | close Lan Mouse on Ubuntu while a Windows session is active | cursor-hide and cursor-park removed; Ubuntu cursor visible | `scripts/qa-handoff-shutdown.sh` covers the daemon; manual for the GUI close |
| M6 | 30 min idle session with occasional movement near (not through) the shared edge | no `LeaveAt` / return requests; cursor hide heartbeat stable | journal count = 0 hand-off events without an actual crossing |

Rollback criterion: any M1/M2/M3 count above one event per intentional
crossing → the position-aware filter or the re-entry gate is not effective in
the real OS path; capture the journal + both trace files and report to the
Lead before shipping.

## 8. Residual risks

1. **Early band exit after an unusually large forward warp.** The position
   filter disarms once the accumulated *forward* displacement reaches
   `HANDOFF_ENTRY_BAND_PX = 16 px`. The largest observed leading forward warp
   is 12.39 px, so the band has margin; a hypothetical >16 px forward warp in
   the same burst as a backswing would disarm the filter and could leak one
   `LeaveAt`. M1/M3 in the manual matrix watch for this.
2. **Bounded escape is intentional.** A reverse gesture is suppressed at most
   until the 250 ms position bound (or until the pointer clearly moved
   inward); an intentional immediate hand-back is delayed, never lost. M4
   checks the real feel.
3. **Mixed-build still needs the same-build candidate.** The receiver deadband
   ignores *immediate* entrance residual and the 250 ms settle, but an old
   0.2.18 source forwards every residual and can still trip a return after the
   settle (the sensitivity test demonstrates exactly that). The fix is
   guaranteed for the same-build 0.2.19 pair; a 0.2.18 Windows peer must be
   upgraded.
4. **Model-based verification.** The replay drives the real `CaptureTask`, but
   the two receiver endpoints are independent models of the shipping code
   (current libei tracker and 0.2.18 Windows backend). Real Mutter/libei,
   real Windows raw-input/hook and Wi-Fi delivery still need M1–M6.
5. **Legacy (`along=None`) receiver anchor.** The 2 px legacy inset counts as
   inward travel for the receiver work deadband, so a legacy peer relies
   mostly on the 250 ms entrance settle; a same-build 0.2.19 pair negotiates
   `EDGE_COORDINATE`, so this only affects deliberately mixed old peers.
6. **No publish / no CI.** The 0.2.19 candidate is a local release build
   validated on this host; nothing was pushed, published or released. The
   release decision (and the version bump commit `ac728e5`) belongs to the
   Lead.
