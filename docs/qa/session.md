# QA ledger — session/runtime core (task-4)

Scope: src/{main,lib,connect,listen,client,service,capture,emulation,coalesce,capture_test,emulation_test}.rs

**Wire compat: unchanged.** No ProtoEvent byte layout change, no new ProtoEvent/IPC variant introduced
by this task (FrontendRequest::Shutdown is additive, landed by protocol-auditor). Behaviour-only
changes: `Leave(serial!=0)` is used as a retry token (the u32 serial was already part of proto v1;
v0.2.4 peers ignore the value and still release) and `AuthRequest` is edge-triggered instead of
repeated per Pong.

## Pass 1 findings (all fixed)

| ID | Sev | Location | Symptom | Root cause | Fix | Regression test | Status |
|----|-----|----------|---------|------------|-----|-----------------|--------|
| S-01 | P1 | src/main.rs:153-162 | Closing the GPUI GUI leaves the process running forever as a daemon (a second service is started after the child was killed) | the `#[cfg(not(feature="gtk"))]` fallback-daemon block also compiles when `gpui` is enabled (default features), so it ran after the frontend exited | cfg = `all(not(gtk), not(gpui))` | `cargo check --all-targets` (default/gpui) + `--no-default-features` both green | FIXED |
| S-02 | P1 | src/main.rs:108-120,139-151 | GUI close can hang forever if the daemon does not exit after SIGINT; `wait()?` error skips kill/reap; Windows path never reaps | unbounded `Child::wait`; error propagation instead of cleanup | `shutdown_service()`: try_wait first (never signal a reaped pid), SIGINT (unix), bounded 25 ms poll up to 3 s, SIGKILL fallback, reap; infallible | `cargo test -p lan-mouse --bins`: already_exited / cooperative / stubborn-child tests | FIXED |
| S-03 | P1 | src/listen.rs:446-527 | Pairing window is "single use" but two concurrent handshakes could both be adopted | the verify callback records intent; the window is consumed after the handshake without checking the `take()` result | `take() == None` now rejects: `finish_session` + close transport, no pin/Accept | `connect::tests::concurrent_pairing_adopts_at_most_one_device` | FIXED |
| S-04 | P1 | src/capture.rs:390-405,683-688 | Panic `no such capture` when an event for a destroyed capture handle is still queued (edge shared by Default + incoming EnterOnly, Destroy overtakes the queued copy) | `get_pos`/`get_type` used `.expect` on a lookup that can legitimately miss | lookups return `Option`; stale events are logged+dropped | `capture_handle_tests::{lookups_of_a_destroyed_capture_return_none, an_event_for_a_destroyed_capture_is_ignored}` | FIXED |
| S-05 | P1 | src/emulation.rs:466-499 | Dead incoming DTLS sessions are never closed: each silent/abandoned peer holds a session until the 128 cap rejects every new incoming connection until restart | no session teardown on the listen side; only the emulation handle was released | on the 3 s silence timeout also `listener.disconnect(addr)` (closes transport + session bookkeeping) | `connect::tests::silent_incoming_session_is_closed_after_the_timeout` | FIXED |
| S-06 | P2 | src/service.rs remove_client | Deleting a connected client (or config hot-reload / reverse-merge) left the outgoing DTLS transport + ping/receive tasks alive; slab handle reuse could make a new client inherit the stale transport | only the manager entry was removed | `CaptureRequest::DropTransport` -> `LanMouseConnection::drop_transport` (silent, no AuthRevoke, no reconnect because the entry is removed first) | code path review + tests still green | FIXED |
| S-07 | P2 | src/service.rs:933-980 + src/connect.rs send_auth_request | One identical `peer_permissions` audit record (blocking file open) + full client-state broadcast every ~2 s per link forever: 4148 of 4189 lines in the user's live audit log | `AuthRequest` was sent on every `Pong`; service audited unconditionally | sender: edge-triggered on changed bits + 30 s refresh (lost-datagram recovery); service: audit+set+broadcast only when the bits change (keyed by fingerprint) | `connect::tests::permission_announcement_is_not_repeated_on_every_pong` | FIXED |
| S-08 | P2 | src/capture.rs:593-604 | Stopping the service while capturing never released the grab: no synthesized key-ups, no modifier reset, no `Leave` -> peer keeps phantom-held keys/buttons | cancellation branch broke without `release_capture` | release_capture best-effort in the cancellation branch before break | code review; manual T9 | FIXED |
| S-09 | P2 | src/service.rs run() | SIGTERM (systemd stop / desktop logout) killed the daemon without teardown (no key release, stale IPC socket) | only `ctrl_c()` was awaited | persistent unix SIGTERM signal stream selected in the run loop; same single teardown block as SIGINT | daemon-level: see QA harness run (kill -TERM -> "terminating service" + clean exit) | FIXED |
| S-10 | P2 | src/service.rs spawn_hook_command | enter/leave hooks silently never ran on Windows | `sh -c` is Unix-only | `cmd /C` on Windows | cfg review | FIXED |
| S-11 | P2 | src/emulation.rs Enter arm + src/service.rs ReleaseNotify | Re-entry/oscillation window: `ReleaseNotify` cleared the remote-peer suppression and `Entered` was published only after awaiting the Ack, so injected input could start while a local begin was still accepted; both sides pushing at the border could enter/leave/enter | transient `set_remote_peer(None)` between remote hook events + async gap before suppression | remove the transient clear; enqueue `Entered` before the awaited Ack | event-order + suppression paths reviewed; Dummy backends cannot reproduce real edge injection (manual T9) | FIXED |
| S-12 | P2 | src/capture.rs:549-580 + src/service.rs ReturnHandoff | A lost `Leave` confirmation left the peer's remote-suppression latched (cursor stuck): the confirmation was sent once and a retried request while no capture was active got no answer | no request/retry state machine for the return hand-off | service retries `Leave(serial!=0)` every 300 ms (max 8) until `PeerLeft`/session end; the controller re-confirms a retried request even when it no longer holds a capture | `service::tests::*` (5 ReturnHandoff tests) | FIXED |
| S-13 | P2 | src/service.rs update_hostname/update_port/update_fix_ips | Endpoint edits while connected left the old transport alive while the UI state claimed the new endpoint (or none): edits never took effect until a manual reconnect | the setters changed state but the live transport was not told | drop the live transport when the resolved endpoint changes; the still-active client reconnects through the normal loop | code path review | FIXED |
| S-14 | P3 | src/listen.rs handshake-permit branch | Handshake-permit exhaustion dropped the accepted conn without closing it | missing `conn.close()` | close before continuing | limiter tests unchanged | FIXED |
| S-15 | P2 | src/service.rs FrontendRequest::Shutdown | The installer/upgrader has no way to stop the daemon gracefully (it `taskkill /F`s it -> stuck-key risk) | request variant did not exist | flag + break out of the run loop -> the one existing teardown path runs (capture keys/Leave release, clipboard, emulation, resolver, IPC socket Drop); idempotent with SIGINT/SIGTERM (single break, teardown once) | CLI `shutdown` waits for the IPC connection to close; daemon-level QA run | FIXED |
| S-16 | P3 | src/client.rs add_with_config/set_dns_ips | A legacy config with an AAAA address made the CLI/UI show an IPv6 endpoint (and kept IPv6 in `dns_ips`) even though the product is IPv4-only and the dialer filters it | the initial state and the DNS setter copied addresses without the IPv4 filter that `update_ips` applies | filter IPv4 in the initial state and in `set_dns_ips` | `client::tests::ipv6_addresses_never_reach_the_client_state` | FIXED |
| S-17 | P2 | src/service.rs EmulationEvent::PortChanged | A runtime port change was not persisted: after a restart the service came back on the old port while peers were configured for the new one (network audit P2-09) | no config write on the successful port-change path | `config.set_port(port)` + `save_config()` on success; error path keeps the old port (nothing persisted) | daemon-level: port persists across restart (config write verified in QA logs) | FIXED |

## Pass 2 (adversarial) — reviewed, no new P1/P2 introduced

- Every remaining production `expect` in scope: RwLock `expect("lock")` (poison-only), channel-closed
  expects reachable only during ordered shutdown (terminate before drop), DTLS downcast (invariant:
  conns map only ever holds DTLSConn). None reachable from network data without a prior panic.
- `update_incoming`'s `expect("no such client")` was audited: `incoming_conns`/`incoming_conn_info`
  are always mutated together (add_incoming / remove_incoming / supersede loop), so it cannot miss.
- Retry state machine: bounded (8 retries), cleared on `PeerLeft`/Disconnected/remove_incoming,
  confirmation when no capture is active does not touch capture state (no double release, key-ups
  are produced only by a real `release_capture`).
- Rate limiting of the new `AuthRequest` path: `auth_announced` is cleared on disconnect so a new
  transport announces immediately; a lost announcement is refreshed within 30 s.
- New async spawns: none added except the per-select signal/interval futures (owned by the service
  loop, dropped with it). No new orphans.
- Hot reload: config self-write still does not trigger a reload (config.rs content compare), and a
  real external reload now closes transports instead of leaking them.

## Verification

| command | result |
|---|---|
| `cargo test -p lan-mouse --lib` (baseline before changes) | 172 passed |
| `cargo test -p lan-mouse --lib` (after fixes) | **201 passed, 0 failed** |
| `cargo test -p lan-mouse --bins` | **3 passed** (shutdown helper: already-exited / cooperative / stubborn-SIGKILL) |
| `cargo test -p lan-mouse --test qa_ip_family --test qa_connection_state --test qa_installer_clean` | **7 + 3 + 5 passed** (contract tests unchanged) |
| `cargo check -p lan-mouse --all-targets` (default = gpui) | green; no warnings in the files of this task |
| `cargo check -p lan-mouse --no-default-features` (headless daemon arm) | green |
| `rustfmt --edition 2021 --check` on all changed files | pass |
| `cargo clippy -p lan-mouse --all-targets` | no warnings in the files of this task (remaining warnings at freeze time belong to src/clipboard.rs, src/audit.rs and lan-mouse-ipc, owned by other agents) |
| `scripts/qa-two-instances.sh` (QA_PORT_A=43991 QA_PORT_B=43992, scenarios `dual duplicate peer_down no_spin`) | **10/11 checks** — see QA note below; no stray daemons left afterwards |
| SIGTERM (real daemon, isolated XDG, port 43993) | `kill -TERM` -> exit 0, log contains `terminating service ...`/`service exited!`, IPC socket file removed |
| CLI `shutdown` (real daemon + IPC) | `lan-mouse cli shutdown` exit 0; daemon logs `shutdown requested over the frontend IPC` + full teardown; socket removed |
| port persistence (S-17) | after a successful runtime port change the config file is rewritten with the new port |

### QA two-instance note (environment + stale scenario)

The one failing check is the `duplicate` scenario's `B criou cliente reverso no port default 4242`
(and its `B tem uma única linha` companion). It is **not a regression from this task**:
- this sandbox already runs a real lan-mouse instance on the host, so 4245/udp is owned by it and
  both harness instances log `network discovery disabled (bind 4245/udp failed)` — the harness
  cannot learn the peer's announced port;
- the scenario pre-dates the IPv4-only product decision: B is configured with `ips = ["::1"]`,
  which is now deliberately never dialed, so B keeps the configured row and adopts A's incoming
  IPv4 session as a *second* row. Its reverse client is created **inactive** (no dial to 4242; the
  documented "unknown listen port => inactive" behaviour) and only displays the default port.
  With discovery enabled the port would have been announced; the check is stale for IPv4-only.
- `dual` (1 row, stable 127.0.0.1 endpoint, stable across reconnect) and `peer_down`
  (stops reporting connected immediately) and `no_spin` (120 ms CPU / 3 s) pass.

## Files changed (this task only)

src/main.rs, src/listen.rs, src/capture.rs, src/emulation.rs, src/connect.rs, src/service.rs,
src/client.rs, docs/qa/session.md.

## Residual risks / manual validation

- Real cursor return (T9 matrix) still needs Windows/GNOME hardware: libei cannot observe injected
  absolute position, Windows absolute-return was never executed at HEAD. The retry/confirmation
  state machine makes a *lost* Leave recoverable, but it cannot fix a backend that never detects
  the edge.
- If all 8 retries AND the peer's confirmation are lost, the latch remains until the next user
  movement (UDP loss probability is negligible; the window is bounded and logged).
- Permission announcements are now edge-triggered; a single lost AuthRequest is refreshed within
  30 s (permission display only; enforcement uses the local policy store).
- GUI-close behaviour not exercised here (no GPUI binary in the sandbox); the cleanup helper is unit
  tested with real child processes.
- Windows-specific code (console std-handle restore, `cmd /C` hooks) is cfg-gated and only
  compile-reviewed on this Linux host.


## Red-team round (independent verifier, 2026-11-09 (post-freeze)) — 3 findings, 3 fixed

| ID | Sev | Location | Symptom | Root cause | Fix |
|---|---|---|---|---|---|
| RT-06 | P2 | main.rs restore_console_std_handles | Windows: redirected/piped stdio was discarded (doctor --json to a file produced an empty file) because AttachConsole succeeds when the parent has a console even with redirected handles | unconditionally replaced stdout/stderr/stdin with CONOUT$/CONIN$ | only install console handles when the existing handle is absent/invalid (GetStdHandle + is_invalid/null check); redirection is preserved |
| RT-07 | P2 | service.rs abort_return_handoff + emulation.rs | one lost Leave(0) from a v0.2.4 peer could end in a forced session teardown carrying AuthRevoke (explicit user disconnect), so the peer deactivated the device until manually re-enabled | reuse of Emulation::disconnect, which signals explicit user disconnect | new Emulation::close_session / EmulationRequest::CloseSession: close transport WITHOUT AuthRevoke (peer watchdog resets capture); real revocations keep using disconnect |
| RT-08 | P3 | service.rs ReturnHandoff::request | a new crossing could inherit an almost-exhausted retry budget | attempts incremented for the same entry regardless of serial | reset attempts when the serial (episode) changes; accumulate only for same-serial retries |

Note: the earlier statement in this ledger about the exhaustion latch is superseded by RT-07.


## Post-release round (v0.2.6 user report) — R-01 fixed

| ID | Sev | Location | Symptom | Root cause | Fix |
|---|---|---|---|---|---|
| R-01 | P1 | src/service.rs disconnect_fingerprint, src/listen.rs, src/emulation.rs | Closing the connection on the receiving side (GPUI revoke/deny with disconnect) left the peer "connected" with its physical mouse frozen: the incoming DTLS session stayed alive (UDP still established, peer kept refreshing permissions every 30s) and the controller never received AuthRevoke, so its capture/cursor stayed latched | disconnect_fingerprint only closed addresses present in incoming_conn_info (capture zones). A connected peer that had not started controlling this machine has no zone there, so the listener session was never closed | new LanMouseListener::disconnect_fingerprint (identity -> logical address) and EmulationRequest::DisconnectFingerprint; the emulation loop sends AuthRevoke and closes every session of that fingerprint, and the service calls it in addition to the capture-zone loop. Both revoke and deny paths use it. |

Regression coverage: scripts/qa-two-instances.sh scenario "revoke" (starts two isolated
instances over IPv4 loopback, connects A->B, revokes B's peer, asserts A becomes
disconnected, B has no live session and the device is denied) + unit test
emulation::tests::session_addrs_match_by_full_fingerprint_identity.
Also verified manually: closing on the controller side (deactivate) already tore the
receiver's session down correctly.
