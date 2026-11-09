# QA ledger — protocol / IPC / discovery / crypto / CLI (task-3)

Owner: protocol-auditor. Write scope: `lan-mouse-proto/`, `lan-mouse-ipc/`,
`lan-mouse-cli/src/lib.rs` (added mid-task by Lead), `src/{crypto,dns,discovery,trusted}.rs`,
this file. Baseline: HEAD `6708b79`, clean tree, workspace build/tests green
(Lead). All changes below are in the working tree (no commits, per instructions).

Prior context read: AGENTS.md, DOC.md, README (IPC/discovery/keys sections),
docs/audit-network.md, docs/audit-ip-family.md, docs/audit-windows-permissions.md,
docs/connection-permissions-plan.md, docs/client-connection-state.md (state
table), docs/qa-repro-matrix.md.

**Wire compatibility statement:** the network wire is **unchanged** (no
lan-mouse-proto byte layout, event id, capability or PROTO_VERSION change; only
tests were added). The local IPC is **additive**: one new unit variant
`FrontendRequest::Shutdown`; older readers skip an unknown frame, older
services answer `Error`/ignore it, so no IPC_VERSION bump is required.

## Findings by severity

Legend: found/fixed. "verified-safe" = investigated, no defect.

| ID | Sev | Location | Symptom | Root cause | Fix | Regression test | Status |
|----|-----|----------|---------|------------|-----|-----------------|--------|
| IPC-01 | P2 | lan-mouse-ipc/src/listen.rs `broadcast` | A slow/stuck frontend froze the whole service loop (docs/audit-network.md P2-06). A partial `Write::write` could interleave two JSON frames and corrupt the line protocol. | sequential `await write()` with no timeout, no `write_all`; failure only dropped the write half | all clients written concurrently with `write_all` under a 250 ms timeout; failed/timed-out clients are shut down and pruned | `broadcast_reaches_every_connected_frontend`, `frontend_connection_cap_is_enforced_and_recoverable` | fixed |
| IPC-02 | P2 | lan-mouse-ipc/src/listen.rs `poll_next` | Read and write halves were unrelated collections: a normal frontend disconnect leaked its write-half fd until the next failed broadcast, and a read-side `Err` item (e.g. invalid UTF-8 line) was dropped on the floor, possibly losing the waker (frontend requests could visibly hang) | no per-client id, `if let Poll::Ready(Some(Ok(..)))` silently ignored `Err`/`None` from `SelectAll` | paired read+write halves under a client id; `Closed` removes both, invalid UTF-8/oversized frames close only that connection; unknown JSON frames are still surfaced and the connection stays alive | `malformed_json_line_does_not_kill_the_connection`, `invalid_utf8_line_closes_only_that_connection`, `oversized_line_closes_only_that_connection` | fixed |
| IPC-03 | P2 (security, Lead report) | lan-mouse-ipc/src/listen.rs `new_at` | IPC socket mode was 0775 (measured by Lead): under a permissive/odd XDG_RUNTIME_DIR another local user could open it (peer-UID check still rejects, but defence in depth was missing) | `UnixListener::bind` result not chmod'ed | `set_permissions(0o600)` after bind (logged, non-fatal); the single accept path (`peer_is_same_user` via `SO_PEERCRED`) remains the enforcement | `unix_socket_file_is_owner_only` | fixed |
| IPC-04 | P2 (DoS, local) | lan-mouse-ipc/src/listen.rs | Unbounded line length and unbounded accepted connections (docs/audit-network.md P2-05) could exhaust memory/fds | `BufReader::lines()` + no connection cap | per-line 1 MiB cap (custom `LineStream`, bounded buffer, oversize closes only that peer) and 16-frontend cap | `oversized_line_closes_only_that_connection`, `frontend_connection_cap_is_enforced_and_recoverable` | fixed |
| IPC-05 | P3 | lan-mouse-ipc/src/listen.rs `new_at` | Any probe failure (even `EMFILE`) deleted the socket file of a possibly running service, stealing its IPC path | unconditional `remove_file` after any connect error | only `NotFound/ConnectionRefused/ConnectionReset/ConnectionAborted` count as stale; other errors return new `IpcListenerCreationError::Probe` | `listen::tests::stale_socket_errors_are_recognised`; existing `stale_socket_file_is_replaced` | fixed |
| IPC-06 | P3 | lan-mouse-ipc/src/connect.rs `wait_for_service` | `connect()` retried forever; a GUI whose service cannot start hung forever with no window and no error (`ConnectionError::Timeout` was dead code in the sync path) | unbounded loop | bounded 30 s wait via testable `retry_until`, returns `Timeout`; non-retryable errors (permission/argument problems) propagate immediately instead of waiting | `retry_until_returns_the_first_success`, `retry_until_gives_up_with_timeout_instead_of_hanging`, `backoff_is_capped_at_one_second`, `non_retryable_connect_errors_propagate_immediately` | fixed |
| IPC-07 | P3 | lan-mouse-ipc/src/connect_async.rs | Async IPC reader returned a hard error on an unknown event variant, unlike the blocking reader that skips undecodable frames; also had no built-in response timeout (CLI used it) | `serde_json` error propagated | skip-and-warn unknown frames; new additive `next_event(Option<Duration>)` helper returning `Ok(None)` on timeout | `async_reader_skips_unknown_events_and_times_out`, `blocking_reader_skips_unknown_event_variants` | fixed |
| IPC-08 | P3 | lan-mouse-ipc/src/listen.rs accept loop | A persistent accept error (e.g. EMFILE) stopped accepting until an unrelated event woke the service, and the previous retry attempt could busy-spin | error path returned `Pending` without registering a waker | one-shot 100 ms retry timer polled from `poll_next` | code review (hard to trigger portably) | fixed |
| IPC-09 | P3 | lan-mouse-ipc/src/lib.rs | `FrontendRequest::Shutdown` missing (Lead request, Windows installer hard-kills the daemon) | feature gap | additive unit variant; CLI `shutdown` waits ≤5 s for the service to close the connection; service handler owned by session-auditor | `frontend_requests_roundtrip` (Shutdown), `shutdown_subcommand_parses` | fixed (IPC+CLI side) |
| DISC-01 | P2 | src/discovery.rs `LocalInterface::host_range` | Subnet scan missed up to half of a /24: with the local address' last octet ≥ 128, hosts below `local-127` were never probed (Network tab incomplete) | start was `max(network+1, local-127)` although the prefix is already clamped to ≥ /24 | start at `network+1`, end at `broadcast-1`, still capped at 254 hosts | `host_range_covers_the_whole_subnet_when_the_host_is_high_in_it` | fixed |
| DISC-02 | P2 (security) | src/discovery.rs `beacon_task` | Unauthenticated discovery port could be used as a UDP reflector (8-byte probe → ~200-byte reply) and as a fake-device flood into the service queue (unbounded announcements, spoofable hostname/fingerprint) | no rate limiting, every datagram handled | per-source token bucket for replies (100 ms/ source, bounded map, fail-closed when full), per-source announce limit (1/s) + global 20/s budget | `source_rate_limiter_allows_one_reply_per_interval`, `source_rate_limiter_memory_is_bounded`, `per_second_budget_resets_each_second` | fixed |
| DISC-03 | P3 | src/discovery.rs beacon `recv_from` | A persistent socket error (Windows WSAECONNRESET from ICMP) made the beacon loop spin without yielding | immediate `continue` | log + 50 ms sleep backoff | code review (OS-specific) | fixed |
| DISC-04 | P3 | src/discovery.rs `probe_host` | An ICMP-unreachable surfaced by `send` (Windows) hid a live host | `send(...).ok()?` mapped it to "no answer" | unreachable send errors map to `HostProbe::HostOnly` | code review (the same mapping covers the existing `recv` path) | fixed |
| DISC-05 | P3 | src/discovery.rs `beacon_task` | After a DHCP/roaming address change, our own announcements could be listed as a remote device (self-detection set snapshot at startup) | `own_addresses` computed once | refreshed on every announcement tick (10 s) | code review | fixed |
| CRYPTO-01 | P1 (Windows, Lead report) | src/crypto.rs `restrict_windows_key_acl` | `icacls /inheritance:r /grant:r {USER}:R` had no post-condition check and no rollback: if the grant fails after inheritance removal the owner may be unable to read the PEM, breaking the next start (docs/audit-windows-permissions.md §2 [P1]) | best-effort hardening, failures only logged | reopen the file after the ACL change; on unreadable/failed icacls restore inherited permissions (`/inheritance:e`) best effort and log loudly. Cannot be executed/tested on this Linux host; Win10/11 manual validation required | existing `generate_then_load_returns_the_same_certificate` runs on Windows CI | fixed by code review; needs Windows validation |
| CRYPTO-02 | P3 (security) | src/crypto.rs `generate_key_and_cert` | Key PEM was created with the process umask and only then chmod'ed to 0400 (brief world-readable window) | `File::create` + later `set_permissions` | create with `OpenOptionsExt::mode(0o400)` (atomic), re-check mode after write; separate `write_private_file` helper. The certificate/private key are one PEM file; there is no separate world-readable cert path (`--cert-path` does not exist at HEAD) | `key_file_is_not_world_readable` | fixed |
| CRYPTO-03 | P3 | src/crypto.rs `certificate_fingerprint` | `expect("certificate missing")` on an empty chain | unchecked `first()` | verified unreachable: `Certificate::from_pem` requires ≥1 CERTIFICATE block and `generate_self_signed` always adds one; left as-is (fail-fast is safer than an empty identity) | — | verified-safe (residual) |
| DNS-02 | — | src/dns.rs `event()` | `event_rx.recv().await.expect("channel closed")` would abort the service if every sender vanished | resolver task holds an `event_tx` clone and only exits on terminate (which is not followed by `event()` calls), so it is unreachable while the resolver is alive; documented as residual instead of changing the signature (call site is in src/service.rs) | — | — | verified-safe (residual) |
| DNS-01 | P3 | src/dns.rs `terminate`/`do_dns` | `terminate()` panicked when called twice or when the task failed; finished resolution handles were never pruned (slow growth over create/delete cycles) | `expect`/no pruning | idempotent tolerant `terminate` (logs a join error); prune finished tasks when a new request arrives | code review | fixed |
| TRUSTED-01 | — | src/trusted.rs | No defect: prefix bounds, family separation, broad-range flag, malformed rejection, display roundtrip | — | no change | existing tests | verified-safe |
| PROTO-01 | — | lan-mouse-proto/src/{lib,clipboard}.rs | No panic/alloc path found: truncation, unknown ids, oversized chunk, invalid mime all error; every variant fits `MAX_EVENT_SIZE`=21; clipboard chunk 1024+16 fits DTLS DEFAULT_MTU 1200 (no IPv4 fragmentation) | — | added fuzz-style random-bytes tests + NaN/inf + future-version tests | `arbitrary_datagrams_never_panic`, `arbitrary_frames_never_panic`, `non_finite_motion_values_are_carried_verbatim`, `future_protocol_versions_and_capabilities_are_preserved_and_harmless` | verified-safe |
| CLI-01 | P2 | lan-mouse-cli/src/lib.rs `execute` | `cli activate 99` / `deactivate 99` / `remove-client 99` printed nothing and exited 0 for a nonexistent handle | service silently ignores those requests for unknown handles and the CLI never read replies | pre-flight `Enumerate` + clear `no client with id N ...` error, non-zero exit | need integration test (see residual) | fixed |
| CLI-02 | P2 | lan-mouse-cli/src/lib.rs `List` | `cli list` printed nothing when no clients were configured | no empty-state branch | prints `no clients configured` (scan prints `no devices found on the local network`) | — | fixed |
| CLI-03 | P2 | lan-mouse-cli/src/lib.rs | Commands that expected a reply (`list`, `devices`, `pending`, `history`, `clipboard status`, …) could hang forever against a hung service; fire-and-forget commands printed success even when the service replied `Error`/`NoSuchClient` (e.g. invalid fingerprint on `allow`, `trust`, `deny`, `revoke`, `set-fingerprint`) | unbounded `rx.next()` loops; no reply consumption | new `next_event` (10 s bound, `Error` → non-zero) and `settle` (200 ms window consuming `Error`/`NoSuchClient`/`State` after pure requests); every arm now reports failures before printing success | CLI unit tests for parsing/validation; IPC-level timeout test existing | fixed |
| CLI-04 | P2 | lan-mouse-cli/src/lib.rs `parse_duration` | `cli deny --for 18446744073709551615d` panicked with "attempt to multiply with overflow" (debug) / wrapped (release) | unchecked `value * 60 * 60 * 24` | `checked_mul` with clear error | `deny_durations_do_not_overflow` | fixed |
| CLI-05 | P3 | lan-mouse-cli/src/lib.rs | `--port 0` accepted; `--percent 0/600` accepted (service silently clamped to 10..=500); `history --limit 0/huge` silently changed by the service; `set-host 0 ""` resolved an empty hostname; IPv6 `--ips` values were silently ignored downstream | no client-side validation | `validate_port` (rejects 0), `validate_sensitivity` (10..=500), history limit clamped to 1..=500 with a warning, empty hostname means "clear", `validate_ips` drops IPv6 with a warning (error if nothing remains) | `invalid_ports_and_sensitivities_are_rejected` | fixed |
| CRYPTO-04 | P3 | src/crypto.rs `load_certificate` | A corrupted/replaced PEM file was read with unbounded `read_to_string` | no size check | reject PEM files > 1 MiB before reading (a real key+cert bundle is ~2 KiB) | code review (cert load path is covered by `generate_then_load_returns_the_same_certificate`) | fixed |

## Cross-scope / report-only (not edited here)

- **PROTO_VERSION consequence at HEAD (explicit):** the version is sent in
  `Hello` and displayed by `doctor`/UI, but nothing compares it
  (`lan-mouse-proto/src/lib.rs:19`, `src/connect.rs:988`; docs/audit-network.md
  P3-14). Because the receive loops treat an undecodable/unknown event id as
  "log and skip that datagram" (`src/listen.rs:819-837`,
  `src/connect.rs:1025+`), **version skew is not a crash/desync risk**: an
  unknown newer frame is dropped and the DTLS stream stays healthy; the
  consequence is limited to silently ignoring a new feature plus a UI/doctor
  display of the peer version. The two real hazards are (a) a newer peer
  sending a *known* id with a changed payload layout, and (b) trusting a
  capability bit without gating behaviour - both are protocol-design rules, not
  current code bugs. No gate added here (call sites are out of scope).
- **Windows IPC has no authentication** (loopback TCP 127.0.0.1:5252 cannot
  carry peer credentials): documented in README "Local IPC security" and
  docs/audit-network.md P2-05. A named pipe with a DACL for the current user
  (or a per-session token file with restrictive ACL) is the real fix; Lead
  deferred it. This pass only added the low-risk mitigations: 16-connection
  cap, 1 MiB line cap, framed/non-blocking broadcast, socket mode 0600 on Unix.
- **firewall/lan-mouse.xml only opens 4242/udp** while discovery needs
  4245/udp (docs/audit-network.md P2-08); forwarded to Lead / crossos-auditor.
- `src/service.rs:405 handle_frontend_request` uses
  `request.expect("frontend listener closed")`; the IPC listener never returns
  `None` (it registers the accept waker instead), so this is currently
  unreachable, but it is a latent panic in the service loop. Reported for
  session-auditor.
- `remove_client` in `src/service.rs:2595` emits `Deleted(handle)` even when
  the handle did not exist, so the CLI cannot distinguish "deleted" from
  "nothing to delete" from the reply alone; the CLI pre-checks with
  `Enumerate`. A cleaner fix is for the service to reply `NoSuchClient` there
  (session-auditor's file) - reported, not edited.
- Discovery beacon replies are still unauthenticated by design; the UI already
  marks unknown fingerprints as unverified and nothing auto-authorizes a
  beacon fingerprint at HEAD (`src/service.rs` announce handling only updates
  the network table; docs/connection-permissions-plan.md M2 kept).

## Verification log

```
cargo test -p lan-mouse-proto                      # 26 passed (was 19)
cargo test -p lan-mouse-ipc                        # 37 passed (was 23)
cargo test -p lan-mouse-cli                        # 10 passed (was 7)
cargo test -p lan-mouse --lib                      # 203 passed, 0 failed (shared tree)
cargo clippy -p lan-mouse-proto -p lan-mouse-ipc -p lan-mouse-cli --all-targets
                                                   # no warnings
rustfmt --edition 2021 <every changed file>        # applied
cargo check -p lan-mouse --lib                     # green after my src/ changes
cargo test -p lan-mouse --lib discovery::          # 10 passed
cargo test -p lan-mouse --lib crypto::             # 7 passed
cargo test -p lan-mouse --lib dns::                # 3 passed
cargo test -p lan-mouse --lib trusted::            # 9 passed
cargo check -p lan-mouse-ipc -p lan-mouse-cli -p lan-mouse-proto \
    --target x86_64-pc-windows-msvc                # green (validates cfg(windows)
                                                   # listener/connect/CLI code)
rustc --target x86_64-pc-windows-msvc --emit=metadata <extracted
    restrict_windows_key_acl + write_private_file>  # green: the Windows ACL /
                                                   # private-file code type-checks
cargo check -p lan-mouse --lib --target x86_64-pc-windows-{msvc,gnu}
                                                   # blocked by missing cross C
                                                   # toolchain (ring/cc-rs), not by
                                                   # our code - Windows validation
                                                   # stays manual
```
The root crate's own test target was temporarily red from other agents'
in-flight edits to `src/service.rs`/`input-*`; final workspace verification is
the Lead's step. Files changed in my scope: see `git status`.

## Residual risks / manual validation

1. Windows: key-ACL verify+rollback and the whole private-key path need a real
   Win10/11 run (we can only compile-review it on Linux). Check
   `%LOCALAPPDATA%\lan-mouse\lan-mouse.pem` after first start: owner R, no
   inherited group access, and a restart reuses the same fingerprint.
2. Windows installer: `"$INSTDIR\lan-mouse.exe" cli shutdown` waits ≤5 s for
   the IPC connection to close; against pre-variant services it times out (no
   reply) - the NSIS fallback must force-kill afterwards.
3. Discovery reflection mitigation bounds spam per spoofed source address but
   cannot authenticate announcements; a real cookie/nonce exchange would need
   an additive beacon field (peer must ignore unknown JSON fields - it does).
4. IPC socket 0600 + peer-UID check protect Unix; Windows loopback TCP still
   accepts any local user (named-pipe fix deferred).
5. CLI pre-flight `Enumerate` has a tiny race with a concurrent delete; the
   resulting command is simply a no-op (as before) rather than a false error.
6. No real network was exercised (no GUI/input capture, per rules); discovery
   and clipboard MTU conclusions remain code-level (prior audit measured them).
7. `--cert-path` exists (root binary/config, not the CLI) and points the same
   single PEM (private key + public certificate) elsewhere; the atomic 0400
   creation applies there too, so the key is never world-readable. There is no
   separate public-certificate file to leave at 0644.

## Red-team round (independent verifier, 2026-11-09 (post-freeze)) — 2 findings, 2 fixed

| ID | Sev | Location | Symptom | Root cause | Fix |
|---|---|---|---|---|---|
| RT-09 | P3 | lan-mouse-ipc/src/listen.rs broadcast/FrontendConnection | a frontend that stops reading while sending requests could keep its READ half (and fd) alive after the write half was dropped, bypassing MAX_FRONTENDS and the fd-exhaustion cap | only the write half was dropped; the read half stayed until the peer closed/sent data | shared AtomicBool per connection; broadcast failure sets it; FrontendConnection returns Ready(None) and releases the fd |
| RT-10 | P3 | lan-mouse-cli/src/lib.rs Create batch | cli create with an invalid fingerprint plus --activate printed "activated" and exited 0 while the service rejected the update | settle() returned on the first State (first update) before the batch Error was read | new settle_updates() reads the whole settle window and surfaces Error/NoSuchClient before Activate, then settles the activation |
