# QA audit — input-emulation + input-event

> Task: **task-2** (emulation-auditor). Scope: `input-emulation/**` and
> `input-event/**` only (+ this ledger). Baseline before any edit: working
> tree build green, `cargo test -p input-emulation -p input-event` green.
> Wire compatibility: **unchanged** — no `lan-mouse-proto` / `input-event`
> type or byte-layout change was made. IPC compatibility: **unchanged** (no
> `lan-mouse-ipc` edit).
>
> Method: two full read passes of every file in scope (not grep-driven), then
> an adversarial pass over every `unwrap/expect/panic/let _ =/as-cast` and every
> `cfg` branch. Pure unit tests only (dummy/recording backends, no real input
> emission, no GUI). Cross-target compile checks used the installed
> `x86_64-pc-windows-msvc` and `x86_64-apple-darwin` targets (dormant code compiled,
> not run).

## 0. Result summary

| Severity | Found | Fixed | Open/residual |
|---|---|---|---|
| P1 (crash/hang/stuck input/data loss) | 6 | **6** | 0 |
| P2 (wrong observable behavior/state/lifecycle) | 9 | **9** | 2 documented residual risks |
| P3 (edge case/robustness/telemetry) | 13 | **12** | 1 deferred (cross-crate doctor surface) |

Verification at end of work (all executed):

```
cargo test -p input-emulation -p input-event          # 41 + 5 passed, 0 failed
cargo clippy -p input-emulation -p input-event --all-targets --all-features  # 0 warnings
cargo check -p input-emulation --target x86_64-pc-windows-msvc   # ok
cargo check -p input-emulation --target x86_64-apple-darwin      # ok
cargo check -p input-emulation --no-default-features [--features x11|wlroots|libei|remote_desktop_portal]  # ok
```

## 1. Findings (fixed unless marked otherwise)

| ID | Sev | Location (file:line at HEAD+fix) | Symptom | Root cause | Fix | Regression test |
|---|---|---|---|---|---|---|
| E-01 | P1 | input-emulation/src/wlroots.rs:160,228,236,247 | Whole service panics on a wire input event whose button `state` is not 0/1 or whose `axis` is not 0/1 (button/axis are raw u32/u8 on the wire) | `ButtonState::try_into()?` / `Axis::try_into()?` errors were swallowed by `unwrap_or_else(\|_\| panic!(...))`; auth'd/malformed peer => panic kills capture+network+IPC | Numeric matching for button state (0=release, else press, like every other backend), explicit `Axis::try_from` with `Err(())` drop + warning; no panic on the hot path | unit-testable conversion logic exercised via backend tests; no panic path left (grep) |
| E-02 | P1 | input-emulation/src/wlroots.rs:167-176 | `panic!("wayland protocol violation")` kills the app | Compositor/protocol error treated as panic | Return `EmulationError::Wayland` so the service disables emulation and keeps running | compile + code review |
| E-03 | P1 | input-emulation/src/lib.rs:98-105,185-218,258-306,346-356 | **Stuck mouse button** after peer disconnect/watchdog timeout/Leave: the receiving OS keeps the button down (drag/selection locks) | Only keyboard keys were tracked/released per handle; pointer buttons were forwarded and forgotten | Per-handle `pressed_buttons` tracking; `release_keys` now emits button-up for every held button (plus keys + modifier reset) on `destroy/terminate` | `tests::destroy_releases_held_keys_and_buttons`, `release_attempts_every_held_input_when_one_fails` |
| E-04 | P1 | input-emulation/src/x11.rs:22-50,66-100,178-205 | X11 receiver **process exit** when the peer pressed a key whose evdev->X keycode is out of the server range (e.g. KEY_MICMUTE 248 -> 256; any unmapped out-of-range code) | `key + 8` was sent to `XTestFakeKeyEvent` unchecked; X server raises BadValue and Xlib's default error handler calls `exit(1)` | Query `XDisplayKeycodes` at creation, drop out-of-range keycodes with a one-shot warning; also verify the XTEST extension at creation and fail the backend (falls through to the next one) instead of silently doing nothing | `x11::tests::keycodes_outside_the_server_range_are_rejected`, `keycodes_are_shifted_by_eight` |
| E-05 | P1 | input-emulation/src/macos.rs:505-515 | macOS receiver panics on a button event when CGEvent creation for the cursor position fails | `self.get_mouse_location().unwrap()` in the button path | `let Some(...) else { warn; return Ok(()) }` | macOS target compiles; needs macOS hardware for runtime validation |
| E-06 | P1 | input-emulation/src/macos.rs:55-70,120-185 | Press+release arriving faster than the repeat task's first poll **hangs the emulation loop forever** (cancel awaits a task that never saw the notification) while the task keeps injecting the key -> stuck key + frozen app | `Notify::notify_waiters()` is not queued: a notification sent before the task first awaits `notified()` is lost; `cancel_repeat_task` then awaited the task forever | Per-task `AtomicBool` cancel flag checked before the first wait and on every loop iteration (notification only optimises latency) | macOS target compiles; logic documented; needs macOS hardware |
| E-07 | P2 | input-emulation/src/lib.rs:185-218,368-383; windows.rs:311-313; macos.rs:696-700 | Holding a key on the controlling machine repeats **once** on Linux receivers (wlroots/libei/X11/xdp): Windows->Ubuntu direction broken (prior audit F3) | `InputEmulation::consume` unconditionally dropped repeated key-down events, and only Windows/macOS synthesize repeat | New trait flag `synthesizes_key_repeat()`; Linux backends get the sender's repeated key-down events passed through (capture backends already emit them: WM_KEYDOWN repeats, macOS session-tap autorepeat, libinput/EIS repeats, compositor repeats to layer-shell). Windows/macOS keep their synthesis and still filter duplicates | `backends_without_native_repeat_receive_the_senders_repeats`, `backends_with_native_repeat_do_not_receive_duplicate_presses` |
| E-08 | P2 | input-emulation/src/windows.rs:270-280,551-577 | Windows receiver re-sent **modifier keys** on repeat (holding Ctrl produced repeated VK_CONTROL), unlike a real keyboard (prior audit F11) | Repeat task spawned for every pressed key | Only non-modifier/non-lock keys spawn the repeat task (`is_repeatable_linux_key`) | helper reviewed + Windows target compile; needs Windows hardware |
| E-09 | P2 | input-emulation/src/x11.rs:116-130 | An unknown/side mouse button from the peer was emulated as **left click** (could start a drag) | `_ => 1` fallback in button mapping | `_ => return` (drop unmapped button); state normalised to `state != 0` | code review + Windows/macOS drop behaviour cross-checked |
| E-10 | P2 | input-emulation/src/x11.rs:39-46,172-181 | Fast wheel scrolling lost ticks (240/480 units still produced one click); naive multiplication would allow a hostile peer to queue millions of clicks | Discrete 120-unit events only used their sign | Emit `clamp(abs(value)/120, 1, 10)` wheel clicks per event (cap prevents remote DoS) | `discrete_scroll_is_multi_tick_and_capped` |
| E-11 | P2 | input-emulation/src/subpixel.rs:18-53 | A single NaN/inf motion delta from a corrupted packet **permanently froze the cursor** for the rest of the session (NaN poisoned the remainder) | Non-finite `f64` went into the accumulator; `NaN - NaN = NaN` forever | Sanitise non-finite deltas to 0 and re-sanitise the remainder to `(-1,1)` | `non_finite_deltas_do_not_poison_the_accumulator`, `huge_deltas_do_not_poison_the_accumulator` |
| E-12 | P2 | input-emulation/src/absolute.rs:20-27,160-216; windows.rs:75-77,166-176,285-335 | Return hand-off (Windows client) could keep sending `Leave` every 250ms forever while the peer ignores it; no per-hand-off state | Only a time throttle, no latch/ack | `EdgeReturnLatch`: throttled, bounded (3 attempts) per contiguous edge contact; re-armed when the pointer returns inside, cleared on create/destroy/terminate (= Leave/PeerLeft/disconnect/watchdog) | `edge_return_requests_are_throttled_and_bounded`, `edge_return_latch_rearms_after_the_pointer_returns_inside`, `edge_return_latch_restarts_its_budget_for_a_different_edge` |
| E-13 | P2 | input-emulation/src/macos.rs:620-643 | A wire keycode above 0xFFFF was truncated to u16 and could press a **different valid key** on the receiver | `key as u16` | `u16::try_from(key)` with warning + drop | macOS target compiles |
| E-27 | P2 | input-emulation/src/libei.rs:37-70,404-412 | After the compositor removes an EIS device (lock/unlock, portal pause, device teardown), the backend kept sending events on the **destroyed** EIS object ids: a libei protocol violation that can tear down the whole EIS connection (all emulation dead until restart) | `DeviceAdded` interfaces were stored and never cleared on `DeviceRemoved` | `Devices::forget_device()` clears the interfaces whose `ei_device` object matches the removed device; fresh interfaces arrive with the next `DeviceAdded` | libei target compiles; needs GNOME/KDE runtime validation |
| E-28 | P2 | input-emulation/src/wlroots.rs:88-104 | Startup could **hang forever** on a compositor whose seat has no keyboard capability: the constructor waited unbounded for a keymap that never arrives | `while keymap.is_none() { queue.blocking_dispatch() }` | Bounded 8x `roundtrip()` wait; warn and continue with pointer-only emulation (add_client already handles a missing keymap) | wlroots target compiles; logic reviewed |
| E-14 | P3 | input-emulation/src/lib.rs:258-306 | A failing release event aborted the remaining releases: several keys/buttons left stuck on a dying backend | Early `?` inside the release loop | Attempt every held input, remember first error, return it after all attempts | `release_attempts_every_held_input_when_one_fails` |
| E-15 | P3 | input-emulation/src/libei.rs:414-423 | `unreachable!()` on an unexpected/unknown EIS event could panic the app | Defensive assumption about the compositor | Log (debug) and continue | code review |
| E-16 | P3 | input-emulation/src/libei.rs:145-150 | `SystemTime::duration_since(UNIX_EPOCH).unwrap()` panic if the clock is pre-epoch | unwrap on system clock | `unwrap_or_default()` | code review |
| E-17 | P3 | input-emulation/src/libei.rs:155-200,283-310; xdg_desktop_portal.rs:96-105 | Unknown scroll **axis** was treated as horizontal; NaN/inf motion/scroll was forwarded to EIS/portal | `_ => horizontal` fallbacks and no finiteness checks | Explicit `scroll_deltas` / `discrete_scroll_deltas` / `portal_axis` validation + finiteness checks (f64->f32 narrowing included) | helper review + crate tests |
| E-18 | P3 | input-emulation/src/xdg_desktop_portal.rs:126-200,214-235; libei.rs:270-280 | u32 wire keycode/button narrowed with `as i32` could wrap negative; a hung portal could block app shutdown in `terminate()` | unchecked casts; unbounded D-Bus awaits | `i32::try_from` + drop; 2s timeouts around `session.close()/receive_closed()` in terminate and AsyncDrop | code review + crate tests |
| E-19 | P3 | input-emulation/src/portal_token.rs (new); xdg_desktop_portal.rs:30-92 | XDP fallback backend discarded the portal restore token: permission dialog on **every app start** (prior audit L2b), and libei/XDP could fight over one token file | Token helpers were private to libei and only libei persisted the token | Extracted 0600/0700 atomic token storage into shared `portal_token` module (unit-tested, legacy-cache migration preserved); XDP persists/restores its own `remote-desktop-xdp.token`; warns when the portal grants fewer devices than requested | `different_portal_backends_use_different_token_files`, token tests (4) |
| E-20 | P3 | input-emulation/src/wlroots.rs:155-166,260-265 | Raw wire `state` u8 forwarded as wl_keyboard state (only 0/1 valid => protocol violation); NaN motion forwarded to compositor | raw casts/enums | Normalise state to 0/1, drop non-finite motion | crate compiles/tests |
| E-21 | P3 | input-emulation/src/windows.rs:363-400 | (Verified prior F1) `SendInput` has **no retry loop** anymore; refusal logs once, no executor block, no per-event log flood | already fixed upstream (3a32aca) | Verified only; added `TODO(windows)` for SPI_GETKEYBOARDDELAY/SPEED (deferred, cross-crate + hardware) | code review |
| E-22 | P3 | input-emulation/src/x11.rs:161-170 | A zero-valued axis event (touchpad frame boundary) clicked the wheel | sign-only branch treated 0.0 as "down" | Drop `value == 0.0` / non-finite axis events | helper tests |
| E-23 | P3 | input-event/src/lib.rs:83-105 | Unknown button display string missed its closing parenthesis (log grepping) | format string typo | match-based display for known/unknown buttons | `button_display_is_well_formed` |
| E-24 | P3 | input-emulation/src/macos.rs:745-760 | Unknown modifier bits in a `Modifiers` event made `from_bits` return None and discarded the **known** bits | `unwrap_or_default` on bitflags | `from_bits_truncate` (unknown bits ignored) | macOS target compiles |
| E-25 | P3 | input-emulation/src/libei.rs:158-200,410-425; xdg_desktop_portal.rs:73-93 | `RwLock::read().unwrap()` / `Mutex::lock().unwrap()` poison would panic all future events | unwrap on poison | `unwrap_or_else(PoisonError::into_inner)` | code review |
| E-26 | P3 | input-emulation/src/lib.rs:185-218 | Keyboard events for a handle that was never `create`d were silently dropped (intended: no unreleasable input) — documented and kept; pointer buttons still forwarded | design | Documented in `KeyUpdate::NotTracked` | `events_for_unknown_handles_are_dropped` |

Also **verified non-bugs** (reported to Lead, not changed here):

- `PeerPermissions` audit spam: cadence = one `AuthRequest` per received `Pong`
  (src/connect.rs:1035-1053; 4 pings/2s round => ~2/s per link), listener converts each to one
  event (src/emulation.rs:302-341), service audits unconditionally. Not input-emulation; belongs
  to session-auditor (dedupe by caps/session or edge-trigger `send_auth_request`).
- Cursor oscillation in the old v0.2.4 log: input-emulation contains **no** Enter/Leave state
  (no `active_emulation`/`PeerEnter`/`PeerLeave` identifiers exist at HEAD); the loop is
  service/capture (ReleaseNotify -> capture.release -> `Leave` -> peer `peer_released` ->
  re-Enter). Reported to Lead/session-auditor. Emulation-side contributor E-12 fixed here.
- F14 macOS side buttons: mapping is consistent with the other backends (evdev BTN_BACK/
  BTN_FORWARD; Windows XBUTTON1/2; X11 buttons 8/9; macOS OtherMouse + MOUSE_EVENT_BUTTON_NUMBER
  3/4). App-side support is an OS/app limitation, not a code bug.

## 2. Residual risks (need real hardware / manual validation)

1. **Repeat rate on Linux receivers** (E-07): the sender's OS repeat rate now drives the
   receiver. A compositor that *also* synthesizes repeat for injected keys should be silent
   because every incoming press restarts its repeat delay (senders repeat far faster than the
   ~500-660ms delay). **Manual check required** on Sway/Hyprland, GNOME (libei), KDE and X11:
   hold Backspace/arrow for 5s — expect continuous repeat, not double rate and not single press.
2. **X11 XTEST**: range check covers out-of-range keycodes and missing extension, but other
   asynchronous X errors could still hit Xlib's default handler (which exits). A scoped
   no-op error handler would need coordination with the X11 capture backend (out of scope).
3. **EIS `DevicePaused`**: the libei backend does not drop events while a device is paused
   ("client must not send events until resumed" per EI). Dropping them could lose a key/button
   release and stick input, so this needs a proper pause/resume state machine (release held
   input on resume) — documented, not changed here; needs GNOME runtime to validate whether
   EIS tolerates the stray events today.
4. **wlroots output backpressure**: when the Wayland outgoing buffer is full the backend still
   deliberately drops events (including a key release) to avoid unbounded buffering => a stuck
   key is possible under sustained overload (connection/net stall). Pre-existing design;
   revisited here but not changed (needs a deferred-reliable-events queue + wayland flush
   integration in the service loop).
5. **libei disconnection does not recover in-place**: after EIS/portal disconnect the backend
   reports an error and the service must issue Reenable to recreate emulation (src/emulation.rs,
   out of scope). Real-world effect: KVM input stays off until the user re-enables, after a
   portal restart.
6. **Portal call failure disables emulation**: a single failed portal D-Bus call propagates as
   `EmulationError` and ends the emulation session (service then waits for Reenable). Malformed
   peer events that the portal validates could therefore disable local emulation; not changed
   (behavioral decision for the service layer).
7. **Pause key on Windows**: scancode enum uses the E1 1D 45 pseudo sequence; the backend can only
   emit E0-prefixed make codes, so the mapping is approximate (unverified without hardware).
   Other uncommon keys inherit the upstream mapping TODOs in `input-event/src/scancode.rs`.
8. **F2 elevated foreground (UIPI)**: `SendInput` refusal is logged once at HEAD (no flood, no
   hang) but there is no `last_error`/doctor surface yet (needs input-emulation API + doctor,
   cross-crate); reported to Lead, deferred.
9. **macOS dormant code** (E-05/E-06/E-13/E-24): compile-checked for `x86_64-apple-darwin` only;
   runtime behaviour needs macOS hardware (TCC granted) to validate.

## 3. Files changed

- input-emulation/src/lib.rs (E-03, E-07, E-14, E-26 + tests)
- input-emulation/src/absolute.rs (E-12 latch + tests)
- input-emulation/src/subpixel.rs (E-11 + tests)
- input-emulation/src/error.rs (E-04 new X11 error variant)
- input-emulation/src/x11.rs (E-04, E-09, E-10, E-22 + tests)
- input-emulation/src/wlroots.rs (E-01, E-02, E-20, E-28)
- input-emulation/src/libei.rs (E-15, E-16, E-17, E-18, E-25, E-27 + terminate timeout)
- input-emulation/src/xdg_desktop_portal.rs (E-17, E-18, E-19 + restore token)
- input-emulation/src/portal_token.rs (new: shared token storage, E-19)
- input-emulation/src/windows.rs (E-08, E-12, E-21 TODO)
- input-emulation/src/macos.rs (E-05, E-06, E-13, E-24)
- input-event/src/lib.rs (E-23 + test)

Tests added: 15 new unit tests (input-emulation 41 total, input-event 5 total; up from
26 + 4). All pure (recording backend / arithmetic), no input emission.

## 3.1 Audit passes

- **Pass 1 (read-every-file):** found E-01..E-26.
- **Pass 2 (adversarial: every `unwrap/expect/panic/let _ =/as-cast/cfg` branch, every
  spawn/loop, every error path):** found E-27 (libei stale destroyed EIS objects) and E-28
  (wlroots unbounded keymap wait). No new P1; previous fixes re-checked (repeat pass-through,
  latch semantics, release-on-destroy order, Windows SendInput path).
- **Pass 3 (convergence):** re-read the final diff of every changed file, re-ran the whole
  verification matrix, grep for remaining panic/unwrap in production code (only tests), no new
  P1/P2. Converged.

## 4. Wire / IPC compatibility statement

**Wire compat: unchanged.** No `lan-mouse-proto` or `input-event` serialized type/layout was
modified; there is no capability bit, version bump or mixed-version requirement from this task.
**IPC compat: unchanged** (no `lan-mouse-ipc` file touched).


## Red-team round (independent verifier, 2026-11-09 (post-freeze)) — 1 finding fixed

| ID | Sev | Location | Symptom | Root cause | Fix |
|---|---|---|---|---|---|
| RT-05 | P3 | libei.rs scroll_deltas | huge-but-finite f64 (e.g. 1e300) narrowed to f32::INFINITY and passed to EIS scroll | finiteness checked before the f32 cast, not after | re-check finiteness after narrowing (Motion path already did) |


## Post-release round (v0.2.9/v0.2.10 user report) - R-09: automatic edge return removed

| ID | Sev | Location | Symptom | Root cause | Resolution |
|---|---|---|---|---|---|
| R-09 | P1 | src/emulation.rs, src/edge_return.rs (removed) | On Ubuntu/Wayland the controlled machine could not hand the cursor back; a first heuristic then produced the opposite bug: ordinary rightward movement immediately returned the cursor to the controller | only the Windows emulation backend implements the edge-return notifier. Linux emulation backends inject relative motion only and the protocol never transfers the receiver's absolute pointer position, so a delta-based gesture cannot distinguish "pushing the pointer out through the edge" from normal in-screen movement | the delta-based gesture was REMOVED after v0.2.10 (false positives outweighed the benefit). Return control with the release bind on the capturing machine (Ctrl+Shift+Meta+Alt, already documented by the app) - the normal Lan Mouse escape hatch - or disconnect. The service keeps the native Windows EdgeReturn path and the single-live-session fallback for backends that do report edges; a truthful absolute-position solution needs a protocol extension and is tracked as future work. |
