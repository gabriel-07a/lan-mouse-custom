# Hand-off and shutdown cursor verification (independent QA)

Independent verification of the two regressions reported for this cycle:

1. **Ubuntu touchpad -> Windows hand-off stutters / enters and leaves twice**
   (task-1);
2. **Closing Lan Mouse leaves the Ubuntu cursor hidden/pinned at the top edge**
   (task-2).

This document records the automated checks, the independent regression audit,
the exact commands and results, the remaining risks, and the Windows/Ubuntu
manual checklist. Written by the verifier: **no production source file was
edited by this verification**; the verifier added `tests/qa_handoff_shutdown.rs`,
`scripts/qa-handoff-shutdown.sh` and this document. One production line was
applied by the Team Lead during verification to fix a pre-existing Windows
compile error found by this audit (§3.3, §6.2).

## Verdict

**PASS**, with residual risks in §6 (real Windows/GNOME/portal runtime is
manual; see §5). The two fixes behave as intended under the automated
contracts, the whole Linux workspace suite is green, and the branch compiles
for the Windows GNU target after the one-line fix found here.

| gate | result |
|---|---|
| `cargo fmt --check` | **PASS** (no diff) |
| `cargo test --workspace --all-features` | **PASS** — 750 passed, 0 failed, 1 ignored (pre-existing X11-display test) |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | **PASS** (no warnings) |
| Windows cross-check (`x86_64-pc-windows-gnu`, zig wrapper) | **PASS** after the one-line fix; before it **FAIL** (`E0282`, pre-existing — §3.3/§6.2) |
| `tests/qa_handoff_shutdown.rs` | **PASS** — 36/36 |
| `scripts/qa-handoff-shutdown.sh` | **PASS** — 28/28 checks |
| headless build (`--no-default-features`) | **PASS** |

Verified tree: `main` @ `1738d41` **plus the uncommitted working-tree fixes**,
diff sha256 `bbddc2c019cafe09a1f66cfd7138eceb705792c1365ab5a1e520fefb19c4bb51`
(identical before and after the verification runs, so the results describe one
immutable tree state). The tree is local-only: nothing was pushed, no CI job was
triggered, no release was created. **Commit the tree before release** — the
results below do not describe `origin/main`.

## 1. Scope and environment

| item | value |
|---|---|
| repository | `/home/gabriel/code/lan-mouse-custom` |
| base commit | `main` @ `1738d41` (13 commits ahead of `origin/main`) |
| working tree | 4 modified production files + 3 new verifier files (§2.4) |
| diff sha256 (verified state) | `bbddc2c019cafe09a1f66cfd7138eceb705792c1365ab5a1e520fefb19c4bb51` |
| host | Ubuntu 26.04.1 LTS, Linux 7.0.0-43-generic, x86_64 |
| toolchain | `rustc 1.99.0` (2026-09-28), `cargo 1.99.0` |
| Windows cross target | `x86_64-pc-windows-gnu` (installed); C compiler/linker = repo-vendored zig 0.15.2 wrappers (see §3.3) |
| date | 2026-11-13 |

The regressions are OS-specific: the source side is GNOME/libei capture on
Ubuntu, the receiving side is the Windows input-emulation/capture path.
Everything that can be made deterministic on this host is automated; the parts
that need a real Mutter/portal session, real input devices or the Windows
backend are in the manual matrix §5.

## 2. Fixes under test

Verified as one working-tree change on top of `1738d41`.

### 2.1 task-1 — Ubuntu touchpad -> Windows hand-off

| item | value |
|---|---|
| file | `src/capture.rs` (+186 / −71 lines) |
| root cause | a **timed 200 ms settle window** after `Begin` dropped *every* pointer motion, so the first real crossing movement was lost (cursor "stuck/hesitant" at the shared edge); the reverse-residual handling also dropped only the very first reverse delta via a boolean, so a multi-delta activation burst (compositor warp + push backswing) still leaked to the receiver and the receiver handed control straight back (`LeaveAt` -> enter/leave ping-pong) |
| fix | remove `ENTER_MOTION_SETTLE` and the time window; add `HANDOFF_RESIDUAL_MAX_PX: f64 = 32.0` and `residual_reverse_px: Option<f64>` armed on `Begin`; suppress **leading reverse deltas until the first forward delta** (bounded budget), then forward the first forward delta immediately with no timer; a reverse push larger than the budget is forwarded (deliberate hand-back). `capture.reverse_motion_dropped` trace now carries `remaining_px` |
| tests added | `capture::capture_handle_tests::the_first_forward_motion_after_a_handoff_is_forwarded_immediately`, `…::the_leading_reverse_burst_after_a_handoff_is_suppressed`, `…::a_reverse_push_larger_than_the_residual_budget_is_forwarded`, `…::after_the_crossing_motion_a_reverse_return_is_forwarded` |
| protocol | none changed: no edits in `lan-mouse-proto`/`lan-mouse-ipc`/`input-event`; `Enter`/`EnterAt`/`Leave` handling including the legacy-peer path is untouched |

### 2.2 task-2 — cursor pinned/hidden after closing Lan Mouse

| item | value |
|---|---|
| files | `src/cursor_hide.rs` (+101 / −8), `src/service.rs` (+151 / −2), `input-capture/src/libei.rs` (+284 / −56) |
| root causes | (a) `CursorHide::drop` removed only the `cursor-hide` heartbeat; the **`cursor-park` marker survived clean shutdown** and the bundled GNOME extension hides the pointer while that file exists (it is only deleted by physical pointer movement) — the cursor stayed hidden at the parked/top edge with no service left; (b) teardown ordering could let backend/portal cleanup fail or be skipped before the markers were removed, and (c) a barrier activation still queued on the EIS stream at cancellation was never released, leaving the compositor holding the pointer at the barrier after the portal session closed |
| fix | `CursorHide::remove_markers()` is unconditional and used by `clear()`, `Drop` and the new `shutdown()` (heartbeat + park marker, idempotent); `Service::run` shutdown is ordered by the pure `shutdown_plan(incoming_sessions, outgoing_capture_active)`: **clear cursor markers first**, then `emulation.remote_leave()` (if a peer drives us), then `capture.release()` (if an outgoing capture owns the pointer), then terminate backends, then a final `cursor_hide.shutdown()`; libei capture now drains queued activations on cancellation (`release_queued_activations`, bounded to 4), releases 1 px into the screen from a validated cursor position, and closes the portal session with **750 ms-bounded** `Disable`/`Close`, treating an already-destroyed session ("Object does not exist"/`UnknownSession`) as success |
| tests added | `cursor_hide::tests::drop_removes_the_parked_cursor_marker_too`, `…::shutdown_removes_markers_written_by_another_instance`, `…::clearing_after_a_return_unparks_the_cursor`; `service::tests::a_clean_shutdown_releases_the_cursor_for_every_session_shape`, `…::the_shutdown_path_clears_the_cursor_state_before_any_backend`; `input_capture::libei::shutdown_cleanup_tests::{a_session_the_portal_already_destroyed_is_treated_as_closed, session_teardown_is_bounded_below_the_launcher_grace, the_release_position_moves_one_pixel_into_the_screen, a_non_finite_cursor_position_is_not_used_for_the_release, the_queued_activation_drain_is_bounded}` |
| protocol | none changed (client-side portal/libei session lifecycle only) |

### 2.3 One-line production fix applied during verification (pre-existing Windows blocker)

`src/cursor_hide.rs:50` in the `#[cfg(not(target_os = "linux"))]` arm:
`let path = None;` -> **`let path: Option<PathBuf> = None;`** (applied by the
Lead after this audit reported it; see §3.3 and §6.2).

### 2.4 Verifier artifacts (new files)

- `tests/qa_handoff_shutdown.rs` — deterministic L2 contract harness (§3.1);
- `scripts/qa-handoff-shutdown.sh` — black-box daemon lifecycle harness (§3.2);
- `docs/testing/handoff-and-shutdown-verification.md` — this document.

## 3. Automated verification

### 3.1 Deterministic harness: `tests/qa_handoff_shutdown.rs`

`capture::CaptureTask` and `service::Service` are private to the binary crate,
so the harness compiles the pure modules into the test crate with `#[path]`
(**read-only**; production files are not modified) and pins the observable
contracts. No sockets, no sleeps, no display; every test writes only its own
temporary directory and is parallel-safe.

| test | invariant | result |
|---|---|---|
| `a_clean_teardown_leaves_no_cursor_hiding_state` | `clear()` removes heartbeat **and** park marker; a clean teardown leaves the extension nothing to hide | PASS |
| `a_process_exit_never_leaves_the_parked_cursor_marker_behind` | destructor (`Drop`) restores a parked cursor even on an exit path that forgets `clear()`/`shutdown()` | PASS |
| `a_crashed_service_cannot_keep_the_cursor_hidden_after_restart` | `CursorHide::new()` deletes `cursor-hide`/`cursor-park` left by a crashed daemon (Linux-only test) | PASS |
| `shutdown_removes_markers_this_instance_never_wrote` | shutdown cleanup is unconditional (files from an older instance) and idempotent | PASS |
| `only_the_owning_handle_can_release_the_hidden_cursor` | a `Leave` for a non-owning handle cannot un-hide a live capture | PASS |
| `parking_is_refused_while_a_capture_owns_the_cursor` | `park()` never downgrades a live capture (the extension restores a parked cursor on first movement) | PASS |
| `repeated_enter_leave_churn_releases_the_cursor_on_teardown` | enter/leave churn never leaves an active hide after teardown | PASS |
| `the_service_heartbeat_fits_inside_the_extension_freshness_window` | service heartbeat (1 s) refreshed at least twice inside the extension freshness window (3 s) | PASS |
| `the_extension_version_is_consistent_everywhere` | `cursor_extension.rs`, bundled `extension.js` and `metadata.json` agree (version 6) | PASS |
| `the_first_motion_after_a_handoff_does_not_wait_for_the_batch_window` | hand-off leading edge bypasses motion coalescing | PASS |
| `a_discrete_flush_does_not_end_the_handoff_burst_window` | a click/key flush keeps the crossing burst; the burst still expires (no packet flood) | PASS |
| `the_handoff_uses_a_bounded_residual_filter_not_a_timed_settle` | regression sentinel: a timed settle (`ENTER_MOTION_SETTLE`) must not return; the residual budget is a named, bounded (0 < px <= 64) constant | PASS |

The `#[path]` include also re-runs the modules' own unit tests in the
integration binary (cursor_hide 10, coalesce 14), so the run reports 36 tests.

```sh
cargo test -p lan-mouse --test qa_handoff_shutdown
# test result: ok. 36 passed; 0 failed; 0 ignored
```

Limitation: this compiles copies of the pure modules into the test crate; it
pins their behaviour but not the service wiring. The wiring is covered by §3.2
(black box) and §5 (manual).

### 3.2 Black-box daemon lifecycle: `scripts/qa-handoff-shutdown.sh`

Runs the real headless daemon (`lan-mouse daemon`, dummy capture/emulation
backends, isolated `HOME`/`XDG_*`) and asserts the only artifacts the bundled
GNOME extension consumes:

- `$XDG_RUNTIME_DIR/lan-mouse/cursor-hide` — fresh heartbeat => hide;
- `$XDG_RUNTIME_DIR/lan-mouse/cursor-park` — parked cursor marker => hide at
  the edge until the pointer moves.

The extension keeps the cursor hidden while `cursor-park` exists, so a marker
left behind by a shutdown keeps the pointer invisible with no service running.
On a host with another Lan Mouse instance the harness uses its own `port` and
`discovery_port` and never talks to that instance.

| scenario | expectation | pre-fix | result |
|---|---|---|---|
| `stale_start` | markers left by a dead daemon are removed on next start | PASS | **PASS** |
| `sigterm_release` | SIGTERM (systemd stop / logout) exits 0 and removes both markers | **FAIL** (park survived) | **PASS** |
| `cli_shutdown_release` | `lan-mouse cli shutdown` exits 0 and removes both markers | **FAIL** (park survived) | **PASS** |
| `crash_restart` | SIGKILL cannot clean up (documented window); next start does | PASS | **PASS** |
| `idle_no_hide` | an idle daemon never publishes a hide heartbeat | PASS | **PASS** |

```sh
cargo build -p lan-mouse --no-default-features
scripts/qa-handoff-shutdown.sh            # RESULTADO: PASS (28 checks)
QA_KEEP=1 scripts/qa-handoff-shutdown.sh sigterm_release   # keep dirs/logs
```

The pre-fix failure reproduced exactly the reported symptom:
`[FAIL] SIGTERM: cursor-park removido (ficheiro ainda existe …)` — exit 0 and
`terminating service` logged, heartbeat removed, park marker left behind.

### 3.3 Whole-workspace gates (final tree, after the one-line Windows fix)

```sh
cargo fmt --check
# exit 0, no diff

cargo test --workspace --all-features
# exit 0 — 750 passed, 0 failed, 1 ignored (x11::tests::connects_to_the_x_server,
# requires a display; pre-existing)

cargo clippy --workspace --all-targets --all-features -- -D warnings
# exit 0, no warnings

# Windows cross-check — no mingw gcc on this host, repo-vendored zig 0.15.2
# wrappers, same recipe as docs/testing/clipboard-verification.md §5:
CC_x86_64_pc_windows_gnu="$PWD/target/qa-verify/zig-cc-windows-gnu" \
AR_x86_64_pc_windows_gnu="$PWD/target/qa-verify/zig-ar-windows-gnu" \
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$PWD/target/qa-verify/zig-cc-windows-gnu" \
cargo check --workspace --all-targets --exclude lan-mouse-gtk \
    --target x86_64-pc-windows-gnu
# exit 0 — Finished dev profile
```

The Windows gate **failed before the one-line fix** with exactly:

```
error[E0282]: type annotations needed for `std::option::Option<_>`
  --> src/cursor_hide.rs:50:13
   |
50 |         let path = None;
   |             ^^^^
...
58 |             .and_then(|path| path.parent())
   |                              ---- type must be known at this point
error: could not compile `lan-mouse` (lib) due to 1 previous error
```

Provenance (git blame): the `None` arm predates this cycle (`389093d`), but
commit `06e7fa0` "fix(cursor): park the cursor left at the edge after a remote
return" (2026-11-13 09:02) added the `park_path = path.as_ref()...` chain that
makes the type ambiguous. On Linux the other `cfg` arm gives `path` a concrete
type, so the branch had been **Windows-uncompilable since `06e7fa0`** without
anyone noticing: the branch is 13 commits ahead of `origin/main` and was never
pushed, so the `windows-2022`/`windows-2025` CI jobs never ran on it. A scratch
worktree (`HEAD` + only that annotation) passed the full Windows workspace
check, which proves it was the only cross-platform blocker; the Lead then
applied the line to the production tree and the final run above is green.
This was found by independent verification, not by the two fixes; it is a
release blocker that is now fixed (§6.2).

### 3.4 Specific behaviours requested by the Lead

| requirement | evidence | result |
|---|---|---|
| hand-off: no 200 ms settle | `ENTER_MOTION_SETTLE` absent from `src/capture.rs`; sentinel test `the_handoff_uses_a_bounded_residual_filter_not_a_timed_settle`; only a bounded 32 px residual budget remains | PASS |
| leading reverse burst suppressed | `capture::capture_handle_tests::the_leading_reverse_burst_after_a_handoff_is_suppressed` (workspace run, ok) | PASS |
| first forward motion immediate | `…::the_first_forward_motion_after_a_handoff_is_forwarded_immediately` (ok) + coalescer leading-edge test (§3.1) | PASS |
| shutdown removes both `cursor-hide` and `cursor-park` | `sigterm_release` and `cli_shutdown_release` black-box scenarios (28/28) + §3.1 tests | PASS |
| no stale portal-session latch | `input_capture::libei::shutdown_cleanup_tests` (5 tests, ok) + code review (§4.4): bounded Disable/Close, already-destroyed sessions tolerated, queued activations released on cancel before `session.close()` | PASS (unit/code) — real Mutter portal is manual (§5 T12) |

## 4. Independent regression audit

### 4.1 Hand-off enter/leave churn

- `EnterState` serial/ack contract unchanged: new serial per capture, a stale
  serial cannot ack the in-flight one, retransmission `clamp(2*rtt, 25..100 ms)`,
  give-up after 2 s; a lost `Enter` is repaired by pairing with the next input
  (`ENTER_PAIR_THROTTLE` 5 ms). The fix does not touch this path.
- `Leave` semantics unchanged: `Leave(serial != 0)` is an explicit return
  request; `Leave(0)` from a serialized-leave peer is a confirmation and must
  not cancel a new capture; legacy peers keep using `Leave(0)` as the release
  request.
- Remote-begin suppression unchanged: while a peer drives this machine only the
  incoming `EnterOnly` capture may react to a `Begin`.
- Residual reverse motion: boolean single-drop replaced by the bounded filter
  described in §2.1. The first **forward** delta is forwarded in the same
  `handle_capture_event` call (no timer); a reverse push beyond the 32 px budget
  is forwarded (real return); once the crossing motion was seen, later reverse
  deltas are forwarded unchecked. Trace event
  `capture.reverse_motion_dropped` now logs `remaining_px`.
- First-motion latency: `MotionCoalescer::arm_leading_edge()` on `Begin` plus
  the 40 ms burst window (unchanged) means the first crossing movement is not
  held by the batching timer.
- Receiver side: the "arm only after an inward move" guard was already removed
  by `1738d41` (it made crossing into Windows require extra push); no new
  receiver-side threshold was added by task-1.
- Wire: no protocol change; `EnterAt`/return-coordinate capability behaviour is
  untouched, so mixed old/new peers keep working.

### 4.2 Force / settle regressions

- The timed settle never returns to `src/capture.rs` (sentinel test). The 200 ms
  motion blackout is exactly what felt like "stuck"; removing it is the
  requested root-cause fix, not another threshold.
- The remaining suppression is bounded and directional: 32 logical px of
  *reverse* motion, only before the first forward delta. A deliberate hand-back
  larger than the budget (single event) is forwarded immediately; after the
  first forward delta nothing is filtered.
- No edge-force constants changed: `RETURN_MARGIN_PX`, `EDGE_ANCHOR_INSET`,
  `MAX_DELTA_PX`, `HistoryEvent`/EDGE margins and the absolute-tracker geometry
  are untouched. No extra "push force" is introduced anywhere in the diff.
- Residual risk: a deliberate *micro-return* (reversing before ever moving
  forward, in small deltas totalling <= 32 px) is swallowed as residual. This is
  the intended trade-off; manual T2/T5 validate the feel.

### 4.3 Cursor hide / park

- Heartbeat is published only while a capture owns the hide (or during its
  4 s leave grace, refreshed every 1 s) and removed on teardown; contract test
  keeps 2 x heartbeat interval <= extension freshness (3 s).
- `park()` is a no-op while an outgoing capture owns the cursor, so the
  extension's "restore on first pointer move" cannot reveal a cursor in the
  middle of a hand-off. `clear()`/`Drop`/`shutdown()` remove the park marker.
- Owner tracking: only the client that entered can release the hide; a
  non-owner `Leave` is ignored.
- Extension version contract: installer constant, `extension.js` and
  `metadata.json` must agree (currently 6); mismatch would make the app report
  the extension as outdated after every update.

### 4.4 Shutdown cursor release

- Ordered teardown (`src/service.rs`): cursor markers are cleared **before** any
  backend/portal teardown, so a hung or failing portal cannot skip them; the
  pure `shutdown_plan` decisions are unit-tested for all session shapes (idle,
  incoming remote-control, outgoing capture, both).
- `CursorHide::Drop` is now a last line of defence (`clear()`), covering exit
  paths that forget the explicit call (panic/abort paths as far as Drop runs).
- libei capture: `Disable`/`Close` are individually bounded at 750 ms (both
  together < the 3 s launcher grace), an already-destroyed session is debug
  (not a warning), non-finite portal cursor positions fall back to "release at
  current position", and queued barrier activations are released (1 px into the
  screen, so the release does not re-arm the capture) before the session is
  closed. This directly addresses the "pointer latched at the barrier after
  close" mechanism.
- Black box: SIGTERM and `cli shutdown` both exit 0 with the graceful
  `terminating service` log and no markers; SIGKILL leaves markers (no
  destructor) and the next start cleans them.

### 4.5 Stale daemon / portal session

- Startup removes stale `cursor-hide`/`cursor-park` (unit test + `stale_start`
  black box) and the service removes a stale IPC socket before binding
  (`remove socket:` debug log; exercised by `crash_restart`, which SIGKILLs the
  daemon and restarts it).
- Clean shutdown removes the IPC socket and exits 0 (all black-box scenarios).
- No orphan daemon appears in headless/daemon mode. The GUI child lifecycle
  (`src/main.rs`) was reviewed, not changed: closing the window keeps the
  daemon, explicit quit sends SIGINT and kills/reaps the child after a 3 s
  grace, and every unexpected frontend exit code keeps the service. The real
  GUI path is manual (T9/T11).
- Portal session release: automated only at the client-library level (§4.4);
  whether Mutter drops the `RemoteDesktop` session is manual (T12).

## 5. Manual Windows <-> Ubuntu checklist

Hardware/OS validation that cannot be automated on this host. Run it after the
automated gates pass, with the same commit on both machines, and record trace
files from both sides.

Enable the hand-off trace (opt-in, see `docs/testing/handoff-trace.md`):

```sh
# Ubuntu
systemctl --user set-environment LAN_MOUSE_HANDOFF_TRACE=1
systemctl --user restart lan-mouse-custom.service
# Windows (PowerShell)
setx LAN_MOUSE_HANDOFF_TRACE 1   # restart Lan Mouse afterwards
```

Analyse with `scripts/analyze-handoff-trace.py`.

### Environments

| id | source | receiver | notes |
|---|---|---|---|
| E1 | Ubuntu (GNOME Wayland, touchpad) | Windows 11 | primary reported scenario |
| E2 | Windows 11 | Ubuntu (GNOME Wayland) | reverse direction (libei receiver) |
| E3 | Ubuntu | Windows 10 | older Windows backend |
| E4 | E1 with two monitors / mixed DPI | | edge span mapping |

### Tests

| id | scenario | steps | expected |
|---|---|---|---|
| T1 | crossing into Windows | cross the shared edge 10x from the Ubuntu touchpad, keep moving the finger after the crossing | Windows cursor starts moving immediately (< ~50 ms), no hesitation/jump back; no repeated `Enter`/`LeaveAt`; Ubuntu cursor hidden during the whole session |
| T2 | crossing back | return from Windows to Ubuntu 10x | Ubuntu cursor reappears at the exit height (return coordinate), no flash, no parked cursor left visible |
| T3 | hand-off churn | 5 fast in/out crossings in a row at the same edge position | no ping-pong; `analyze-handoff-trace.py` shows one controller session per crossing and no `controller.input_gap_exceeded` cluster; no repeated `capture.enter_sent` for one crossing |
| T4 | slow crossing | cross very slowly, stopping with the pointer exactly on the edge | no repeated enter/leave; control starts once and stays |
| T5 | force feel | compare the effort to enter Windows before/after the fix at 20%, 50%, 80% of the edge (including a tiny reverse flick right after crossing, <= ~32 px) | same or less effort; never "push harder"; the tiny reverse flick is the documented residual case — a normal return (larger push) must still work |
| T6 | cursor hide during control | while driving Windows, watch the Ubuntu cursor | hidden; no flash on the Ubuntu screen |
| T7 | close app while controlling Windows | close the Lan Mouse window (or quit) while Ubuntu drives Windows | Windows keys/buttons release (no stuck modifiers), Ubuntu cursor visible immediately, no hidden/parked cursor, no marker files left in `$XDG_RUNTIME_DIR/lan-mouse/` |
| T8 | close app while parked | return from Windows to Ubuntu, keep the cursor parked at the edge, close the app | Ubuntu cursor visible and usable immediately; `cursor-park` removed |
| T9 | daemon stop / logout | `systemctl --user stop lan-mouse-custom.service`, then log out/in | cursor visible, no portal session left, no orphan `lan-mouse` process |
| T10 | crash | `pkill -9 -f 'lan-mouse daemon'` while driving Windows, then restart | restart cleans markers, cursor visible; Windows does not keep keys latched (peer watchdog) |
| T11 | stale daemon | kill the GUI, check for a surviving daemon (`pgrep -af lan-mouse`); relaunch | no surviving daemon after quit; relaunch works and reuses nothing stale |
| T12 | portal session | after T7/T9, inspect the GNOME `RemoteDesktop` indicator / portal session list (e.g. `gdbus call` on `org.freedesktop.portal.Desktop`) | no Lan Mouse `RemoteDesktop`/`InputCapture` session remains |
| T13 | worst-case shutdown timing | with a busy/slow session, quit and time the process exit; check `journalctl --user -u lan-mouse-custom.service` | daemon exits 0 well inside the 3 s grace; cursor restored even if the exit is close to the limit (markers are removed first) |

For every test record: result (PASS/FAIL), OS pair, commit on both machines,
trace files and the relevant `journalctl` excerpt.

## 6. Residual risks and known limitations

### 6.1 Shutdown grace vs worst-case portal teardown (documented follow-up)

The libei capture session teardown is bounded (750 ms per call, 1.5 s total),
and the cursor markers are removed **first** in the shutdown sequence, so the
cursor is restored even if the process is later killed. However, the complete
teardown also terminates the clipboard and emulation backends; with multiple
hung portal/D-Bus calls the total can exceed the frontend's
`SERVICE_SHUTDOWN_GRACE` (3 s), in which case the launcher SIGKILLs the daemon
(skipping the remaining cleanup). Consequences are limited because the marker
files are already removed and keys are released on peer watchdog, but the
shutdown-path completion is not guaranteed. **Follow-up (not part of this
fix): raise `SERVICE_SHUTDOWN_GRACE` (or bound the clipboard/emulation teardown
the same way as the capture session).** Owner: shutdown-fixer / Lead decision.

### 6.2 Windows `E0282` (found here, fixed during verification)

Pre-existing since `06e7fa0`; the branch had never been pushed so CI never
compiled it for Windows. Fixed with the one-line annotation in `cursor_hide.rs`
(§2.3/§3.3). The full Windows workspace check passes after it. This is a process
gap (local-only branch without the Windows CI job) worth noting: run the Windows
cross-check (or push to a branch that triggers CI) before cutting a release.

### 6.3 Hand-off residual model

The suppression budget is a fixed 32 logical px. If a touchpad/compositor
activation burst (warp + backswing) exceeds 32 px before the first forward
delta, the excess reverse motion is forwarded and could still trigger one
receiver-side return (`LeaveAt`). T3/T4 plus the hand-off trace are the intended
watchpoints. The first-forward-delta assumption (residual arrives before user
forward motion) is validated only by unit tests and manual T1/T4.

### 6.4 Not automatable on this host

- Windows capture/emulation backend and real injected pointer (T1-T5, T10, T11).
- GNOME Mutter cursor visibility and the bundled extension's actual behavior
  (T6-T8); only the file contract and version constants are automated.
- Real libei/RemoteDesktop portal sessions and consent tokens (T9, T12).
- GUI window-close -> child SIGINT path and "no orphan daemon" (T9/T11): the
  code was reviewed, the headless daemon path is automated.
- The Windows gate is compile-only (`cargo check`); `windows-2022/2025` CI
  remains the authoritative compile+test target.

### 6.5 Harness limitations

- `tests/qa_handoff_shutdown.rs` compiles copies of the pure modules into the
  test crate: it pins their behaviour but not the service wiring; the black-box
  script covers the Linux daemon wiring.
- The black-box script uses the dummy capture/emulation backends, so it proves
  the marker/lifecycle contract, not the portal or pointer behavior.
- `#[path]` copies can drift from the binary-crate build (e.g. a future
  feature-gated module split); the whole-workspace test run is what exercises
  both.
- `portal_session_already_gone` is heuristic string matching over portal error
  text (locale/version dependent); it only controls log level, not cleanup
  correctness.
- Crash (`SIGKILL`) cannot remove markers: the extension ignores the heartbeat
  after 3 s, but a stale `cursor-park` hides the cursor until the pointer moves
  or the service restarts (T10).

## 7. How to rerun the verification

```sh
# 1. deterministic contracts
cargo test -p lan-mouse --test qa_handoff_shutdown

# 2. black-box daemon lifecycle (builds the headless binary itself)
scripts/qa-handoff-shutdown.sh
# keep artifacts for inspection:
QA_KEEP=1 scripts/qa-handoff-shutdown.sh

# 3. workspace gates
cargo fmt --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings

# 4. Windows cross-check (repo-vendored zig wrappers; no mingw gcc on this host)
CC_x86_64_pc_windows_gnu="$PWD/target/qa-verify/zig-cc-windows-gnu" \
AR_x86_64_pc_windows_gnu="$PWD/target/qa-verify/zig-ar-windows-gnu" \
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$PWD/target/qa-verify/zig-cc-windows-gnu" \
cargo check --workspace --all-targets --exclude lan-mouse-gtk \
    --target x86_64-pc-windows-gnu

# 5. manual matrix §5 on real Windows/Ubuntu machines
```

## Appendix A — raw evidence

Logs for this run (git-ignored): `target/qa-verification/`
`final-fmt.log`, `final-harness.log`, `final-test.log`, `final-clippy.log`,
`final-windows-check.log`, `final-headless-build.log`, `final-script.log`;
the pre-fix Windows failure is in `windows-check-zig.log`.

```
fmt:      exit 0
harness:  test result: ok. 36 passed; 0 failed; 0 ignored
tests:    test result lines summed: 750 passed, 0 failed, 1 ignored
clippy:   Finished, exit 0 (no warnings)
windows:  Finished dev profile, exit 0
headless: Finished dev profile, exit 0
script:   RESULTADO: PASS (28 checks)
tree:     diff sha256 bbddc2c0… unchanged before/after all runs
```

Fix-specific tests observed `ok` in the workspace run:
`capture::capture_handle_tests::{the_first_forward_motion_after_a_handoff_is_forwarded_immediately,
the_leading_reverse_burst_after_a_handoff_is_suppressed, a_reverse_push_larger_than_the_residual_budget_is_forwarded,
after_the_crossing_motion_a_reverse_return_is_forwarded}`,
`cursor_hide::tests::{drop_removes_the_parked_cursor_marker_too, shutdown_removes_markers_written_by_another_instance,
clearing_after_a_return_unparks_the_cursor}`,
`service::tests::{a_clean_shutdown_releases_the_cursor_for_every_session_shape,
the_shutdown_path_clears_the_cursor_state_before_any_backend}`,
`libei::shutdown_cleanup_tests::{a_session_the_portal_already_destroyed_is_treated_as_closed,
session_teardown_is_bounded_below_the_launcher_grace, the_release_position_moves_one_pixel_into_the_screen,
a_non_finite_cursor_position_is_not_used_for_the_release, the_queued_activation_drain_is_bounded}`.
