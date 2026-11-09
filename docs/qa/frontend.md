# Frontend audit ledger — GPUI (default UI) + legacy GTK

Task: **task-6**. Scope: `lan-mouse-gpui/` (excluded workspace crate, shipping default UI on
Linux/Windows), `lan-mouse-gtk/` (opt-in GTK fallback), `docs/qa/frontend.md`.
Baseline: workspace green at `6708b79` (clean tree). Docs read: `DOC.md#Connection State
Shown by the Frontend`, `docs/client-connection-state.md`, `docs/audit-connected-truth.md`,
`docs/gpui-migration.md`, `docs/qa-repro-matrix.md`, README (platform support).

Contract used for verdicts (DOC.md): the device card derives badge/action/subtitle from
`active`, `active_addr`, `incoming_addr`, `alive` **plus a fresh round-trip proof**;
"Connected" requires an outgoing link **and** a recent reply; `alive` is the peer emulation
flag (not liveness); states Disconnected / Connecting… / Connected (amber + "emulation
disabled on the other PC" when `alive=false`) / Connected (bidirectional) / Peer connected;
one action per state (Connect/Cancel/Disconnect/Also connect). GTK additionally has LinkProof
(`last_pong_at` + heartbeat fallback), per-peer aggregation by certificate fingerprint and a
3 s endpoint debounce (audit-connected-truth).

**Verdict on the Lead's question:** GPUI already used `last_pong_at` (6 s window) and never
derived "Connected" from `alive` alone, but it had lost almost every other GTK truth fix —
all regressions are fixed below (FE-01..FE-05, FE-25).

## Findings

Found: **0 × P1, 7 × P2, 22 × P3** (29 total). Fixed: **28**. Open (reported, not a defect): **1**.

| ID | Sev | Location | Symptom | Root cause | Fix | Regression test | Status |
|---|---|---|---|---|---|---|---|
| FE-01 | P2 | lan-mouse-gpui/src/lib.rs (`link_state`/`row_action`, peer card render) | Enabled client with no outgoing link (first attempt, or the automatic retry after a drop — the common post-disconnect state) rendered **"Disconnected" + Connect**, but clicking it sent `Activate(false)` (disconnected). Contradicts DOC "Connecting…". | Old `ClientRow::from_state` only mapped `active && healthy` / `active && active_addr`; the fallthrough arm was Disconnected/Connect; click handler toggled the raw `active` flag instead of the displayed action. | Real `LinkState` enum (Disabled/Connecting/Reconnecting/ConnectedDegraded/Connected/ConnectedBidirectional/Receiving) + `RowAction`; handler applies the displayed action (fan-out to all peer members). | `enabled_client_without_link_is_connecting_and_offers_cancel`, `handshake_without_round_trip_is_not_connected`, `action_table_matches_each_state` | fixed |
| FE-02 | P2 | lan-mouse-gpui/src/lib.rs (state colour + card detail) | `alive=false` on a live link was shown **green "Connected"** with no degradation note; a link that stopped answering stayed "Connected" until the next IPC event; no bidirectional indication. DOC requires amber + "emulation disabled on the other PC" and a bidirectional state. | Derivation used only freshness for the colour and ignored `alive`/incoming; no proof-age state. | Degraded ⇒ amber + note; stale proof ⇒ Reconnecting with seconds; incoming+outgoing ⇒ "link healthy (bidirectional)". | `peer_emulation_disabled_is_degraded_not_green`, `stale_link_is_reconnecting_not_connected`, `fresh_round_trip_is_the_only_way_to_claim_connected` | fixed |
| FE-03 | P2 | lan-mouse-gpui/src/lib.rs (`peer_groups`/`aggregate_peer`) | Two configured clients for one peer (one entry per address family, same certificate fingerprint) rendered as **two cards**; actions touched only one handle. GTK fixed this in audit-connected-truth §2.2. | GPUI rendered one card per service handle. | Group by normalized `ClientConfig::fingerprint`; aggregate active/alive/incoming/last_pong/link fields (prefer a member with a proven round-trip); fan out toggle/edit/DNS/position/sensitivity/delete. Entries without fingerprint stay separate on purpose. | `duplicate_entries_of_one_peer_are_aggregated`, `aggregation_prefers_the_member_with_a_round_trip`, `peers_are_grouped_by_fingerprint` | fixed |
| FE-04 | P2 | lan-mouse-gpui/src/lib.rs (`Dashboard::new`) | No periodic re-render: freshness was computed at paint time but the view repainted only on IPC events, so a silent peer/spawned service kept showing "Connected" indefinitely. GTK has a 1 s status tick for exactly this. | No timer. | 1 s foreground timer calling `cx.notify()` only (state remains service-owned). | GUI-only (code reviewed; no headless render harness) | fixed |
| FE-05 | P2 | lan-mouse-gpui/src/lib.rs (`BridgeMessage`, bridge thread, `send`) | Service IPC EOF (crash/exit) was silent: bridge thread exited, UI kept showing last state + "Connected to service" forever and requests vanished. | Reader loop end was not reported to the UI. | `BridgeMessage::Closed` ⇒ status "Lost connection to the Lan Mouse service — restart Lan Mouse"; writer is dropped, later requests say "Not connected…". | GUI-only (code reviewed) | fixed |
| FE-06 | P2 | lan-mouse-gpui/src/lib.rs (Linux titlebar ✕, `run`) | Custom-titlebar ✕ called `window.remove_window()`: process kept running with a tray whose "Open" cannot restore a removed window — invisible, unquittable app (kill only). Contradicts documented close-to-tray. | ✕ bypassed `on_window_should_close` (minimize). | ✕ now minimizes (same as WM close); tray Quit routes through `cx.quit()` (graceful: launcher stops daemon); Settings has an in-window Quit (FE-09). | GUI-only (needs manual Linux Wayland/X11 check) | fixed |
| FE-07 | P2 | lan-mouse-gtk/src/window.rs (`Window::new`) | GTK fallback never requested `FrontendRequest::Enumerate` (nor `Sync`); the service only pushes `Enumerate` on request, so after a restart the GTK device list was **empty** until an unrelated event (configured/inactive devices invisible). | Missing initial snapshot request (GPUI sends it). | Request `Enumerate` next to `Hello`. | code review + GTK unit suite | fixed |
| FE-08 | P3 | lan-mouse-gpui/src/lib.rs (Settings → Pairing) | "Open pairing window" stayed enabled while a window was active; repeated clicks sent `StartPairing(60)` again (live audit log: 14 × `pairing_open` in one session). GTK already disables/relabels. | GPUI ignored `pairing_active` for the button. | While `pairing_active` render a non-interactive "Pairing…" chip (GTK parity; single audit record per window). | GUI-only | fixed |
| FE-09 | P3 | lan-mouse-gpui/src/lib.rs (Settings → General) | No in-window quit. Closing minimizes (documented close-to-tray) and desktops without a StatusNotifier host (plain GNOME/Fedora, minimal WMs) have no tray Quit ⇒ app could only be killed. | Quit existed only in the tray menu. | "Quit Lan Mouse" button → `cx.quit()` (graceful; launcher stops the daemon). | GUI-only | fixed |
| FE-10 | P3 | lan-mouse-gpui/src/lib.rs, lan-mouse-gtk/src/window.rs (`pending_add`) | Single `pending_add` slot applied to the *next* `Created` without correlation: two rapid adds made both clients apply the second request's address/config; a rejected create left a stale pending applied to a later client. | No in-flight guard, no clear on `FrontendEvent::Error`. | Ignore a second add while one is in flight; clear on `Error`. Same guard in GTK. | GUI-only | fixed |
| FE-11 | P3 | lan-mouse-gpui/src/lib.rs (7 sites) | Fingerprint labels used byte slicing `&fp[..n]`: panics on a non-ASCII boundary. Service normalizes to hex today ⇒ defence-in-depth (GTK used `chars().take()`). | Byte slicing on display text. | `short_fingerprint()` using `chars().take()` everywhere. | `multibyte_fingerprint_truncation_does_not_panic` | fixed |
| FE-12 | P3 | lan-mouse-gpui/src/lib.rs (Settings → Device permissions) | Green **"connected"** badge derived from optimistic/stale `DeviceView.connected` (`alive || incoming`), no round-trip proof (R8). | UI trusted the aggregate flag. | Badge: "connected" only with a fresh `last_pong_at` for the matching fingerprint, else "unconfirmed"/"offline". | `fresh_pong_requires_a_known_and_pinned_fingerprint` | fixed |
| FE-13 | P3 | lan-mouse-gtk/src/lib.rs (IPC reader thread) | On quit the gio worker pool panicked: `sender.send_blocking(e).unwrap()` after the receiver was dropped. | Unwrap on a channel that legitimately closes at shutdown. | Break the loop on send error. | code review | fixed |
| FE-14 | P3 | lan-mouse-gtk/src/window/imp.rs | `hostname.to_str().expect("invalid utf8")` at window construction and in the copy-hostname callback (non-UTF-8 hostnames exist on Unix); copy callbacks also `unwrap()`ed the GDK display. | Panic on fallible OS string / missing display. | `to_string_lossy()`, display guard with toast. | code review | fixed |
| FE-15 | P3 | lan-mouse-gpui/src/lib.rs (tray cfg) | Crate could not compile for macOS with default features: `tray` had Linux/Windows modules + `not(feature="tray")` fallback only ⇒ unresolved `tray::spawn` on macOS (README: macOS unsupported/dormant ⇒ dormant cross-OS breakage). | Missing fallback module for other targets. | Fallback cfg is now `not(all(feature="tray", any(target_os="linux", windows)))`. | compile blocked here (offline registry) — see residual risks | fixed |
| FE-16 | P3 | lan-mouse-gpui/src/lib.rs (sidebar) | "N active link(s)" counted `ClientState.active` (the user's enabled flag), not links. | Wrong counter. | Count peer groups with a fresh outgoing round-trip. | `fresh_round_trip_is_the_only_way_to_claim_connected` (derivation) | fixed |
| FE-17 | P3 | lan-mouse-gpui/src/lib.rs (`send`) | Requests issued before the IPC writer exists were silently dropped with no feedback. | `None` writer ignored. | Explicit status "Not connected to the Lan Mouse service yet". | code review | fixed |
| FE-18 | P3 | lan-mouse-gtk/src/window.rs (`device_subtitle`, `create_device_row`) | Devices panel said **"Receiving"** when `DeviceView.connected && active` — neither proves input is applied (documented R8). | Panel derived from aggregate `connected`/`active`. | Panel uses `device_proof()` (fresh LinkProof for the fingerprint): Connected only with proof, otherwise Unconfirmed/Offline; "Receiving" removed. | `device_panel_never_claims_connected_without_round_trip_proof` | fixed |
| FE-19 | P3 | lan-mouse-gtk/src/window.rs (hostname/port entries) | GTK sends `UpdateHostname`/`UpdatePort` on every keystroke ⇒ one IPC request + config save + State broadcast per char (UI-triggered request churn). | No debounce on text entries. | **Not changed** (behaviour is correct, only chatty; a debounce is QoL/UX, not a correctness fix). Reported for follow-up. | — | open (reported) |
| FE-20 | P3 | lan-mouse-gpui/src/lib.rs (history rows) | History printed raw Unix-ms timestamp. | No formatting. | Human-readable age ("5s ago"/"2h ago"). | `timestamp_age_is_human_readable` | fixed |
| FE-21 | P3 | lan-mouse-gpui/src/lib.rs (`run`) | `open_window(...).expect(...)` panicked (release `panic=abort`) when window creation failed (no compositor/GPU init failure) instead of reporting. | `expect` on fallible startup. | Log the error, `cx.quit()`, return; launcher reports the non-zero/failed frontend. | code review | fixed |
| FE-22 | P3 | lan-mouse-gpui/src/lib.rs (tray Quit) | Tray "Quit" used `std::process::exit(0)` (skips log flush/drop). | Prototype shortcut. | Route through `cx.quit()` so `Application::run` returns and main exits 0 (launcher then stops the daemon). | code review | fixed |
| FE-23 | P3 | lan-mouse-gtk/src/window.rs (`Window::request`) | `frontend_request_writer.as_mut().unwrap()` panics if a callback fires before the writer is installed / after teardown. Not reachable via current construction order (latent). | Unwrap on a state option. | Log + drop the request. | code review | fixed |
| FE-24 | P3 | lan-mouse-gtk/src/lib.rs (event loop) | Service EOF made `process::exit(1)` silently (no log, no message). | Abrupt exit path. | `log::error!` before exiting (a GTK UI without the service is useless; behaviour kept). | code review | fixed |
| FE-25 | P3 | lan-mouse-gpui/src/lib.rs (`sticky_endpoint`) | No endpoint debounce: the service can rewrite `active_addr` per Pong, so a dual-stack peer's card could flap IPv4↔IPv6. GTK has the 3 s sticky endpoint. | Card showed the raw `active_addr`. | Per-group 3 s debounce (`ENDPOINT_STICKY_WINDOW`, same algorithm as GTK); cleared on loss/delete/enumerate. | `endpoint_debounce_does_not_flap_between_families` | fixed |
| FE-26 | P3 | both frontends (scan buttons) | Every click sent `ScanNetwork`; the service spawns a fresh discovery task per request ⇒ concurrent scans/duplicate beacon traffic. | No client-side rate limit (no scan-in-progress event exists in IPC). | 3 s local rate limit + "Scanning…" feedback (GPUI `start_scan`, GTK `request_user_scan`); the startup scan is unaffected. | code review | fixed |
| FE-27 | P3 | lan-mouse-gpui/src/lib.rs (add dialog), lan-mouse-gtk/src/window.rs (`request_port_change`) | Invalid port text silently fell back to the default port (GPUI added the device on 4242; GTK re-created the UDP listener on 4242). | `unwrap_or(DEFAULT_PORT)`. | GPUI keeps the dialog open + status error; GTK shows a toast and ignores; no-op when the port did not change. | code review | fixed |
| FE-28 | P3 | lan-mouse-gpui/src/lib.rs (`handle_event`) | `ConnectionAttempt`/`CertificateChanged` were ignored; pending approval + changed-fingerprint warning were only visible if the user happened to be on the Devices page. | No GPUI modal/toast; events dropped. | Status-line hint ("Connection request from <addr> — review it on the Devices page" / "Certificate changed for <name> — review the pending request before approving"); the pending card still carries SAS + "certificate changed (old: …)" + "Trust new certificate" (never auto-trusted). | GUI-only | fixed |
| FE-29 | P3 | lan-mouse-gpui/src/lib.rs (tray spawn) | `spawn().expect("spawn tray thread")` panicked the UI at startup on thread-spawn failure. | `expect` in fallible path. | Warn and continue without tray (in-window Quit covers exit). | code review | fixed |

Non-findings (checked and deliberately not changed): GTK `row_connection_state`/LinkProof
derivation + all 21 tests (badge never claims Connected without fresh proof; alive ≠ liveness;
sticky endpoint; sorted IPs); GTK `current_action` fallback maps to the same activation bit as
the displayed action; GTK clipboard echo guard (`updating_clipboard`); GTK permission checkbox
`updating_devices` guard; GTK pairing button already disabled while active; GPUI UI-scale/rem
code and bounded content height; GTK `merge_peer_states`/peer grouping; trays reviewed by
reading (Linux SNI D-Bus, Windows Win32 message-only window + `PeekMessageW` pump, macOS
dormant objc) — Windows needs real-hardware validation; service-side races listed in
docs/client-connection-state.md §4 (R4–R7) are service scope, not frontend.

## Verification

```
# GPUI (excluded crate; local target dir)
cd lan-mouse-gpui && cargo check --all-targets          # ok (only dependency future-incompat note)
cd lan-mouse-gpui && cargo clippy --all-targets         # ok (no warnings in the crate)
mkdir -p /tmp/lmlibs && ln -sf /usr/lib/x86_64-linux-gnu/libxkbcommon-x11.so.0 /tmp/lmlibs/libxkbcommon-x11.so
cd lan-mouse-gpui && LIBRARY_PATH=/tmp/lmlibs cargo test
  # 17 passed, 0 failed (dev-env only: the runtime SONAME exists but the dev symlink
  #  libxkbcommon-x11.so is not installed on this machine; no repo change)

# GTK fallback (workspace crate)
cargo check -p lan-mouse-gtk --all-targets              # ok, no warnings
cargo clippy -p lan-mouse-gtk --all-targets             # ok (only a pre-existing lan-mouse-ipc warning from another task)
env -u DISPLAY -u WAYLAND_DISPLAY cargo test -p lan-mouse-gtk --lib
  # 46 passed, 0 failed (template smoke test skips headless; DISPLAY was unset to avoid
  #  touching the live desktop)

rustfmt --edition 2024 lan-mouse-gpui/src/lib.rs
rustfmt --edition 2021 lan-mouse-gtk/src/{lib.rs,window.rs,window/imp.rs,client_row.rs}
```

Tests added: 17 GPUI unit tests (state derivation matrix incl. never-Connected-without-proof,
action/activation table, peer aggregation by fingerprint, endpoint debounce, char-safe
fingerprints, fresh-pong matching, history ages) + 1 GTK test (Devices-panel proof labels);
GTK suite grew 45 → 46.

Wire compat: **unchanged** (no lan-mouse-proto edits, PROTO_VERSION untouched).
IPC compat: **unchanged** (no lan-mouse-ipc type edits; frontends only consume existing
events/fields). All edits are frontend-only.

## Dev-environment note: GPUI test linking on this machine

`cargo test` for `lan-mouse-gpui` needs to link the system `libxkbcommon-x11`. This box has
the **runtime** library (`/usr/lib/x86_64-linux-gnu/libxkbcommon-x11.so.0`) but not the
**dev** symlink `libxkbcommon-x11.so`, so rustc's `-lxkbcommon-x11` fails. This is a host
dependency gap (Debian/Ubuntu package `libxkbcommon-x11-dev`), not a repo defect.

Exact reproduction used for the gate (no repo/system changes; `/tmp` is per-run here, so the
symlink must be created in the **same** shell invocation as the test command):

```sh
mkdir -p /tmp/lmlibs
ln -sf /usr/lib/x86_64-linux-gnu/libxkbcommon-x11.so.0 /tmp/lmlibs/libxkbcommon-x11.so
cd lan-mouse-gpui && LIBRARY_PATH=/tmp/lmlibs cargo test
```

Recommendation for README/build docs: list the GPUI build/ test deps on Debian/Ubuntu as
`libxkbcommon-x11-dev` (plus the usual x11/wayland/fontconfig/freetype dev packages) so
`cargo test` in `lan-mouse-gpui` links without the `LIBRARY_PATH` workaround. `cargo check`
and `cargo clippy` do **not** need it (no linking).

## Residual risks / needs real hardware or manual validation

1. **Windows 10/11 (P2 residual):** tray registration/menu/message pump (`tray-icon` +
   Win32 `PeekMessageW` timeout in GTK), server-side-decorated window close, in-window Quit,
   and the `cx.quit()` tray path can only be validated on real Windows. Cross-check here is
   blocked: gpui needs the mingw gcc toolchain for C deps (not installed) and the cached
   Windows-GNU artifacts predate this change; this audit only exercised frontend-only code.
2. **macOS dormant:** GPUI macOS cross-check is blocked by the read-only/offline cargo
   registry (`objc_id` download fails). The only known macOS compile break (missing tray
   module) is fixed by the new fallback module; the GTK macOS dormant code was read but not
   compiled (as per README macOS is unsupported/dormant).
3. **Wayland close-to-tray:** ✕ now minimizes; tray "Open" calls `activate_window()`, which
   on Wayland only requests an activation token — Mutter/KWin usually unminimizes, but this
   needs a manual Wayland/X11 check. No GPUI hide/show API exists in gpui 0.2.2.
4. **Manual matrix (from docs/audit-connected-truth.md §5):** peer power-off (≤6 s stale
   Connected, then Reconnecting…/Connecting…), emulation disabled on the peer (amber), dual
   entries for one fingerprint (one card), IPv4/IPv6 flap (stable ≥3 s), peer death while
   the UI is on another page (status hint). UI rendering itself is not covered by unit tests
   (no headless GPUI render harness in this crate).
5. GPUI element ids inside a card are static strings shared by every peer card (pre-existing
   pattern, click dispatch is hitbox-based; not changed) — potential state/id-collision smell
   only.
6. GTK sends hostname/port updates per keystroke (FE-19, chattiness/config-write churn) and
   the GTK device/UI template runtime instantiation smoke test only runs when a display is
   available (skipped in headless CI).


## Red-team round (independent verifier, 2026-11-09 (post-freeze))

No new frontend findings: the verifier re-checked GPUI LinkState/RowAction (apply_row_action uses action.wants_activation(), so no displayed action sends the opposite request), peer aggregation, and the GTK request/Enumerate/proof paths — no concrete failure path found.
