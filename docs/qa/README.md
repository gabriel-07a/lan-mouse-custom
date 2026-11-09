# QA hardening pass — final evidence index

Date: 2026-11-08/09 (America/Sao_Paulo). Baseline: `6708b79` (release v0.2.5), clean tree, build+workspace tests green.
Goal: launch-readiness bug hunt across the whole application and all supported OS cases (Linux Wayland/X11,
Windows 10/11; macOS code is dormant/unshipped), fixing real and probable defects.

## Workstreams and ledgers

| Area | Owner | Findings (found / fixed) | Ledger |
|---|---|---|---|
| input-capture backends (Linux Wayland/X11/libei, Windows, macOS) | capture-auditor | 20 / 20 (P1 6) | [capture.md](capture.md) |
| input-emulation + input-event backends | emulation-auditor | 28 / 27 (P1 6, 1 deferred cross-crate) | [emulation.md](emulation.md) |
| protocol/IPC/crypto/discovery/DNS/CLI | protocol-auditor | 23 / 23 (P1 1) + 4 verified-safe | [protocol.md](protocol.md) |
| service/session runtime core | session-auditor | 17 / 17 (P1 5) + 2 post-freeze fixes | [session.md](session.md) |
| config/devices/reset/audit/doctor + clipboard | config-clipboard-auditor | 53 / 52 (P1 5) | [config-clipboard.md](config-clipboard.md) |
| GPUI (shipping) + GTK fallback frontends | frontend-auditor | 29 / 28 (P2 7, 1 reported open) | [frontend.md](frontend.md) |
| cross-OS build/cfg matrix, packaging, installers, CI, docs truth | crossos-auditor | 23 / 21 (P1 6) + 1 documented + 1 deferred | [cross-os.md](cross-os.md) |

A separate read-only cross-review (wire compatibility + session state-machine) was performed by
emulation-auditor; the Lead also ran independent end-to-end daemon/CLI tests and reviewed the IPC,
config, shutdown and hand-off code.

## Verification evidence (final frozen tree)

- `cargo build --workspace --all-targets` — PASS
- `cargo test --workspace` — **495 passed, 0 failed, 1 ignored**, run twice (no flakes). Test count
  grew from 380 (HEAD) to 496 (30% more regression tests).
- `cargo clippy --workspace --all-targets` and `--all-features` — no source warnings (only the
  transitive `proc-macro-error2` future-incompat notice).
- `cargo fmt --all -- --check` — PASS (workspace) and gpui crate.
- `--no-default-features` build + test — PASS.
- GPUI crate (excluded workspace): check / clippy / **17 tests** — PASS.
- Linux feature matrix: **23/23 combos PASS** (default, no-default, gtk/gpui/no-frontend, every
  backend alone, all-features, members).
- Cross-target type-check (cargo check, fake C toolchain — no link): windows-gnu **15 PASS / 3
  expected FAIL** (GTK-only combos), apple-darwin identical, plus post-freeze root-default /
  root-nodefault PASS on both.
- Two-instance end-to-end QA (`scripts/qa-two-instances.sh dual duplicate`, custom ports, isolated
  XDG): **15/15 checks OK** (bidirectional single-row connection, stable address, no reverse client
  on the default port; IPv4-only).
- Binary E2E (isolated XDG, dummy backends): daemon start, IPC socket **0600**, CLI list/add/
  activate/errors, graceful `cli shutdown` (exit 0, socket removed), SIGTERM teardown
  ("terminating service", exit 0, socket removed), `doctor --json`.

## Compatibility

- Network wire format: byte-identical for all pre-existing frames/ids; `PROTO_VERSION` remains 1.
  A v0.2.4 peer keeps working (verified by source-level diff of the production code and legacy-decode
  tests; a mixed-version two-machine smoke test is still worth doing during rollout).
- Local IPC: additive only (`FrontendRequest::Shutdown`); readers skip unknown frames.
- `Cargo.lock` unchanged.

## Residual risks (need real hardware / native CI / real network)

1. Windows 10/11 runtime: tray + message pump, mixed-DPI multi-monitor (100/125/150/200%), tablet/RDP
   absolute input, UIPI/elevated-window input blocking, private-key ACL verification/rollback,
   installer/uninstaller end-to-end (NSIS cannot be compiled on this host; release CI is the gate).
2. macOS is dormant/unshipped: type-checked only (apple-darwin cargo check with fake C toolchain).
3. Cursor return path: the retry/confirmation state machine makes a lost `Leave` recoverable, but
   detection itself depends on the backend (libei/GNOME portal barrier; Windows absolute-return path
   was compile/logic-validated only). Needs a real Windows/GNOME two-machine crossing test.
4. Key-repeat behavior when the receiving OS is Linux (repeat pass-through) — validate on Sway/
   Hyprland, GNOME/libei, KDE and X11 with a real Windows/macOS sender.
5. X11 clipboard INCR both directions + XFixes owner-change detection on a real X server; portal
   restart recovery; Windows clipboard-manager contention.
6. Windows frontend IPC is loopback TCP without authentication (documented; named-pipe + DACL design
   proposed, not implemented). Discovery beacons are rate-limited but unauthenticated (cookie design
   proposed). `PROTO_VERSION` is advertised but not compared (unknown ids are skipped safely).
7. Pairing window is consumed atomically but not bound to the window instance seen at verify time (P3,
   documented). wlroots backpressure can still drop a key-release under sustained overload
   (pre-existing design, documented).
8. Cross-target checks are cargo-check only (no C compilation/link); native CI (windows-2022/2025) is
   authoritative. Authenticode signing is wired but inert without CI secrets.

## Re-running the checks

```sh
bash target/qa-freeze/run-gate.sh               # fmt/build/test x2/clippy/feature combos/gpui
QA_PORT_A=43931 QA_PORT_B=43932 scripts/qa-two-instances.sh dual duplicate
```
Cross-target and feature-matrix harness: `target/qa-matrix/matrix.sh` (gitignored, from crossos-auditor).


## Red-team round (post-freeze)

Two independent verifiers (fresh context, read-only) reviewed the frozen diff after the first full gate.
**11 findings (2 P1/P2-class input bugs + 3 P2 + 6 P3)** — all fixed and re-verified:

- **P1** Windows capture thread was always detached on Drop (Exit never posted; leaked hooks/windows); a drop during capture could leave local input swallowed and the cursor hidden.
- **P2** Windows absolute-device input inverted MOUSE_VIRTUAL_DESKTOP semantics (normalized 0..65535 treated as pixels -> 655 px jumps).
- **P2** one transient WouldBlock from the layer-shell socket permanently disabled Wayland capture.
- **P2** Windows console restore discarded redirected stdout/stderr (empty doctor --json file).
- **P2** return-hand-off exhaustion closed the session with an explicit-disconnect signal, forcing the peer to deactivate the device until manually re-enabled.
- P3: layer-shell position rollback, libei scroll f32 overflow, IPC read-half fd leak after broadcast timeout, CLI create batch could hide a rejected update, audit log recreated 0644 after rotation, vacuous config watcher test.

Re-verified after fixes: workspace build/tests (495 passed, 0 failed, run twice), clippy/fmt clean,
Linux feature matrix, and cargo check for the changed crates on x86_64-pc-windows-msvc,
x86_64-pc-windows-gnu and x86_64-apple-darwin. Binary E2E re-run: IPC socket 0600, CLI create with an
invalid fingerprint now exits 1 instead of printing success, graceful cli shutdown and SIGTERM teardown
(exit 0, socket removed).


## Post-release fix (v0.2.7)

User report after v0.2.6: closing the connection on the receiving side left the peer
connected with the physical mouse frozen (cursor still captured). Root cause and fix in
session.md row R-01: session teardown by certificate identity, including sessions that
never registered a capture zone. Regression: scripts/qa-two-instances.sh revoke.


## Post-release fixes (v0.2.7 user report, not yet released)

- Capture portal (GNOME v1) now creates ONE session per app run and reuses it for every hand-off;
  the permission dialog no longer appears on every activation. At most one prompt per lan-mouse
  launch on GNOME (portal v1 cannot persist consent across restarts); portal v2 persists it.
- Windows: the GPUI frontend no longer allocates a console window.
- Device approval always grants both directions (single Approve action).
See capture.md R-02 and frontend.md R-03/R-04.

- (v0.2.8) Peer side is now an explicit selector on the card and can be changed live on both the
  controller and the receiving side without disconnecting. Regression: qa-two-instances.sh side.

- Closing the connection on ANY machine now completely closes it (outgoing transport, incoming session,
  AuthRevoke to the peer, local suppression cleared). Side edits no longer move a live incoming capture
  zone. Regression: qa-two-instances.sh scenario close.

- Windows: the GPUI frontend now embeds the icon resource, fixing the missing taskbar/pinned-shortcut
  icon; the notification-area icon uses the same embedded Lan Mouse icon. Regression: packaging test
  windows_gpui_frontend_embeds_the_icon_resource.

- The delta-based return gesture introduced in v0.2.10 was removed: on relative-only backends
  (GNOME Wayland) it fired on ordinary cursor movement because the protocol has no absolute pointer
  position. Return control with the release bind (Ctrl+Shift+Meta+Alt) on the machine that holds the
  capture, or disconnect. The native Windows edge reporting is unchanged.
- Closing from either side is reliable: AuthRevoke is retransmitted, the controller never re-dials an
  inactive client, and the listener rejects reconnections of a locally disconnected identity until the
  user allows it again (no "blink back"). Regression: scenarios close/grace.

- A local Disconnect is final: the controller no longer re-dials an inactive client (capture-retry gate),
  AuthRevoke is retransmitted, and the receiver rejects reconnections of that identity at the handshake
  until the device is allowed again (no transport, no injected input, no blink). Regression: scenarios
  close/grace (assert zero new sessions/dials after the disconnect).

- Automatic edge-return on relative-only backends (GNOME Wayland) was removed again: without an absolute
  pointer anchor it fired on ordinary movement. Use the release bind (Ctrl+Shift+Meta+Alt) on the machine
  that holds the capture, or disconnect. Native Windows edge reporting stays.
- A peer you connected to is registered as a trusted device with the bidirectional defaults, so accepting
  it once no longer produces a second permission prompt when it connects back (scenario noprompt).
