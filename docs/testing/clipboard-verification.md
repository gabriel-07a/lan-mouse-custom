# Clipboard atomicity fix: independent verification

Independent acceptance and adversarial verification of the clipboard `Ctrl+C`
reliability fix ("sometimes the first copy on one machine does not reach the
other; pressing Ctrl+C again fixes it", Windows 10/11 ↔ Ubuntu, both
directions). This report is written by the verifier, not by the implementer:
the acceptance harness, the flake checks, the adversarial repros and the
Windows cross-check were executed independently.

Related documents:
[analysis + ranked hypotheses](../analysis/clipboard-atomicity.md),
[OS manual matrix](clipboard-os-matrix.md),
[test guide](README.md), [backend overview](../../DOC.md#clipboard).

## 1. Verdict

**PASS** for the reported symptom and for every standard gate, on the verified
tree below. Two edge-case divergences and one static race found by the
independent audit were reproduced, patched by the implementation, and
re-verified green. Residual risks are listed in §8 and are outside the
reported two-machine scenario or require real Windows/Ubuntu hardware
(§9 manual checklist).

- Verified state: `HEAD 03f2a46` + **uncommitted** working-tree fix,
  `git diff | sha256sum` = `342a9e7d94fd7b38444a3a14457ec0e2c9388e57828633365bf0ba50ccac04da`,
  9 files, +1922/−147. The hash was identical before and after the full gate
  run (no tree movement during verification).
- An earlier iteration of the same fix (`git diff` sha `89091aba…`) passed the
  same gates; the final tree additionally contains the task-5 patches for the
  three adversarial findings below.
- Nothing was committed, pushed, released or sent to CI by this verification.
- No production or test file was edited by the verifier. Only this document
  and git-ignored artifacts under `target/qa-verify/` were written.

## 2. Environment

| Item | Value |
|---|---|
| Host OS | Linux x86_64 (`America/Sao_Paulo`, 2026-11-12) |
| Toolchain | `cargo 1.99.0 (5f94df78 2026-08-27)`, `rustc 1.99.0 (b940084d7 2026-09-28)` |
| Installed cross target | `x86_64-pc-windows-gnu`, `x86_64-pc-windows-msvc`, `x86_64-apple-darwin` |
| Windows linker | No `x86_64-w64-mingw32-gcc` on the host; the repo toolchain `.tools/zig-x86_64-linux-0.15.2/zig` was used as the `cc`/`ar`/linker wrapper (see §5) |
| Real Windows machine | **Not available.** Windows-only code is type-checked, not executed (§8). |
| Display/X11/portal | Headless session: OS clipboard backends cannot be exercised here (`x11::tests::connects_to_the_x_server` is `#[ignore]`d on the pre-existing suite and fails with `--ignored` because there is no display — unrelated to this fix). |

## 3. Acceptance harness (independent, deterministic)

Source: [`tests/qa_clipboard_atomicity.rs`](../../tests/qa_clipboard_atomicity.rs),
runner: [`scripts/qa-clipboard-stress.sh`](../../scripts/qa-clipboard-stress.sh).
The harness drives only public APIs (`input_clipboard::sync::ClipboardSync`,
`FrameSink`, `ClipboardSnapshot`, `Limits`, the production clipboard frame
codec) over an in-memory datagram wire with simulated time, seeded/periodic
loss/duplication, and a fake backend that mirrors the `src/clipboard.rs` service
glue. No sleeps/sockets/threads; deterministic by construction.

| Command | Result |
|---|---|
| `cargo test --test qa_clipboard_atomicity` | **30 passed, 0 failed, 2 ignored** |
| `cargo test --test qa_clipboard_atomicity -- --ignored` | **2 passed, 0 failed** (both residual/robustness cases now green) |
| 20 consecutive default runs | 30 passed each run, identical output |
| 20 consecutive `--ignored` runs | 2 passed each run |
| `scripts/qa-clipboard-stress.sh --runs 20 --with-unit` | all 20 rounds passed; each round also repeats `input-clipboard` and `lan-mouse-proto` clipboard unit tests |
| Thread-count sensitivity (`--test-threads=1`, `4`, `16`) | identical results, no shared-state flake |

The two `#[ignore]`d cases are *not disabled because they fail*: they encode
behaviour beyond the transport's original contract and were red before the fix
(`frames_queued_before_a_disconnect_are_not_applied_after_it`,
`copy_during_a_long_transport_outage_is_recovered_without_a_second_copy`). They
pass now; **recommended follow-up: remove the `#[ignore]` attributes once the
tree is committed** (the verifier was asked not to edit test files during
task-4, so this was intentionally left for the implementation/lead).

### Baseline before ↔ after

| Gate | Before fix (`03f2a46`) | After fix (verified tree) |
|---|---|---|
| Acceptance harness | 28 passed, **2 failed**, 2 ignored | 30 passed, 0 failed, 2 ignored |
| Harness `--ignored` | **2 failed** | 2 passed |
| `input-clipboard` unit tests | 58 passed, 1 ignored | **82 passed**, 1 ignored (same pre-existing X11 display test) |
| `lan-mouse-proto` clipboard tests | 10 passed | **11 passed** |
| `lan-mouse` lib tests | 261 passed | **264 passed** |
| Workspace tests | green except the 2 expected acceptance failures | **all green** |

The two originally red acceptance tests reproduce the reported symptom exactly
and are the core sign-off:

- `reconnect_after_an_interrupted_transfer_resyncs_the_current_clipboard`
- `copying_again_after_reconnect_is_offered_again`

Pre-fix evidence (deterministic, 5/5 identical runs): after a mid-transfer
disconnect the wire send counter stayed at 9 while the service's
read-on-connect resync (`src/clipboard.rs:369-374` before the fix) hit the
global `last_sent` dedupe (`input-clipboard/src/sync.rs:117-120` before the
fix); a second identical `Ctrl+C` was swallowed by the same hash. Post-fix the
state is per-peer (`offered: HashMap<PeerId, [u8;32]>`) plus an unconditional
`offer_current_to` on registration, and both tests pass 20/20 runs.

## 4. Implementation audit (independent review of the final diff)

Reviewed files (all changes are inside task-2/task-5 scopes; no `Cargo.toml`,
`Cargo.lock`, protocol-version or capability change):

`input-clipboard/src/{lib,sync,transfer,portal,windows}.rs`,
`lan-mouse-proto/src/clipboard.rs`, `src/{clipboard,listen,service}.rs`.

### 4.1 Delivery/ack semantics

- **An `Ack` now means "received *and applied*"**: the receiver withholds the
  final acknowledgement for a completed transfer until the service wrote the
  snapshot to the OS clipboard (`on_applied` releases the deferred `Ack`s;
  `on_apply_failed` sends `Cancel` instead). A failed receiver write therefore
  cannot leave the sender believing the copy was delivered.
- **Apply retries are bounded**: `PendingApply` retries a failed
  `backend.write()` up to 7 attempts with growing delays (~4.7 s), then gives
  up with `Cancel` and lets the sender re-offer.
- **Retransmission budget**: `ACK_TIMEOUT` 120 ms with delays
  120/120/240/240/480/480 ms (`MAX_WINDOW_ATTEMPTS = 6`, ≈1.7 s), then an
  automatic sync-level re-offer every 2.5 s (`MAX_RETRIES = 20`, ≈50 s) and
  only then a reported error. The offer is repeated in every retransmission
  round, so a receiver that lost its transfer state can rebuild it.
- **Failure paths are terminal-safe**: failed transfers clear the per-peer
  `offered` marker and schedule exactly one pending re-offer; a newer local
  copy, a newer applied remote copy, peer removal or `reset()` cancels it.
- **Echo loops**: `last_applied` hash + backend own-write filters
  (`windows.rs` sequence/own-write, Wayland `last_written`, X11 selection
  ownership, portal owner checks) suppress re-sending our own applied content;
  harness `applied_remote_content_is_not_echoed_back` + quiet-state assertions
  pass 20/20.

### 4.2 Wire compatibility with peers that lack the new behaviour

- **No wire change at all**: the only `lan-mouse-proto` change is the local
  helper `ClipboardFrame::is_content()` (+ its unit test); `FRAME_*`, `MIME_*`,
  `CHUNK_SIZE`, `MAX_CLIPBOARD_FRAME`, `ACK_WINDOW`, `PROTO_VERSION` and
  `capabilities::*` are untouched (verified: `lan-mouse-proto/src/lib.rs` diff
  is empty). No new capability bit, no new frame type, no layout change.
- **Repeated `Offer`** (new sender behaviour) is already handled by every
  existing clipboard-capable build: the old `handle_frame` re-sends the current
  acks for an offer with the same `(serial, origin, hash)`; duplicate chunks and
  acks are idempotent. Legacy peers therefore keep working.
- **Delayed final `Ack`** (new receiver behaviour) uses a standard `Ack` frame;
  a legacy sender completes when it arrives. If our local write fails, the new
  receiver sends the standard `Cancel`; a legacy sender aborts (it has no
  auto-retry) — see residual risk R-1 for mixed-version pairs.
- **Policy gating fix is frame-class based, not wire based**: `Offer`/`Chunk`
  are still gated by clipboard-read (outgoing) / clipboard-write (incoming);
  `Ack`/`Cancel` are now allowed through because they are control replies for a
  transfer the peer already started. A peer granted write-only clipboard access
  can finally complete a transfer to us (regression test
  `a_write_only_peer_can_send_content_but_is_not_offered_ours`).
- **Serial epochs**: each sync instance starts at a per-origin epoch and a
  repeated offer is only re-acknowledged when serial **and** origin **and** hash
  match, so a receiver holding a transfer from a previous instance (backend
  restart, ≤30 s timeout) can no longer fake-acknowledge a new transfer.

### 4.3 Stale offers / late frames / retries

- `remove_peer` clears the peer's incoming transfer, offer marker, deferred
  acks and pending retry; `reset()` clears all state. Frames are only accepted
  from registered peers (`handle_frame` gate), so a complete transfer that was
  queued before a disconnect can no longer recreate state and be applied
  (finding F2, regression test
  `a_transfer_queued_before_a_disconnect_is_not_applied_after_it`).
- Registered ≠ receivable: a new `may_receive` set separates "peer identity is
  known / its inbound content and control frames are accepted" from "we may
  send our clipboard to it". Write-only/incoming-route peers are registered but
  never offered our clipboard; service glue sets it from the per-route device
  policy for outgoing handles, incoming `Hello` peers and lazily-created
  incoming peers.
- Applying a newer remote snapshot now cancels superseded in-flight outgoing
  transfers and drops a superseded pending re-offer (`drop_superseded_retry`),
  preventing an old outage-retry from overwriting newer content (finding F1).
- A local copy cancels a still-pending remote *apply* (`cancel_pending_apply`)
  so a later successful write of the older remote snapshot cannot overwrite the
  user's newer local copy (finding F3).

### 4.4 Platform-side read robustness

- Windows: `changed()` retries a failed read 3×40 ms; the service additionally
  retries a `changed() -> Err` with `input_clipboard::read_retrying` (4 attempts,
  50/100/150/200 ms) because the native notification is already consumed at
  that point.
- Portal: an owner-change read that comes back empty is retried up to twice
  (60 ms apart, ≤500 ms total).
- Wayland/X11 backends are unchanged except the shared read-retry path; see
  residual risks for the X11 slow-owner and portal owner-is-us semantics.

## 5. Commands and exact results (final tree `342a9e7d…`)

```sh
cargo test --test qa_clipboard_atomicity            # ok. 30 passed; 0 failed; 2 ignored
cargo test --test qa_clipboard_atomicity -- --ignored  # ok. 2 passed
cargo test -p input-clipboard --lib                 # ok. 82 passed; 1 ignored (X11 display)
cargo test -p lan-mouse-proto clipboard --lib       # ok. 11 passed
cargo test -p lan-mouse --lib                       # ok. 264 passed
cargo test --workspace --no-fail-fast               # ok. 693 passed; 3 ignored; 0 failed
cargo clippy --workspace --all-targets --all-features -- -D warnings  # clean
cargo fmt --all --check                             # clean
scripts/qa-clipboard-stress.sh --runs 20 --with-unit  # all 20 rounds passed
```

Windows compile cross-check (affected crates; no Windows runtime available):

```sh
# The two crates with Windows-only code and the protocol crate:
cargo check -p input-clipboard -p lan-mouse-proto --target x86_64-pc-windows-gnu
# -> Finished, no warnings

# Root service crate (src/clipboard.rs, src/listen.rs, src/service.rs):
# no mingw gcc on this host, so .tools/zig 0.15.2 is used as the C compiler,
# archiver and linker, with its caches under target/qa-verify (git-ignored).
CC_x86_64_pc_windows_gnu="$PWD/target/qa-verify/zig-cc-windows-gnu" \
AR_x86_64_pc_windows_gnu="$PWD/target/qa-verify/zig-ar-windows-gnu" \
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$PWD/target/qa-verify/zig-cc-windows-gnu" \
cargo check -p lan-mouse --no-default-features --target x86_64-pc-windows-gnu
# -> Finished, no warnings
```

Not executable here: the Windows clipboard backend at runtime
(`input-clipboard/src/windows.rs`), the real Wayland portal, real X11
selection/INCR, and Windows DPAPI/secure-desktop behaviour. Those are covered
by the manual matrix in [clipboard-os-matrix.md](clipboard-os-matrix.md) §4–§6;
the matrix's S4/S5/S6/S15 rows are exactly the reported-symptom regressions.

Artifacts (git-ignored, under `target/qa-verify/`): full logs
`full-verify.log` (pre-patch iteration), `full-verify-patched.log` (final tree),
`stress.log`, `windows-root-check2.log`, and the ad-hoc adversarial binary
source/rebuild instructions (see §6).

## 6. Adversarial findings (independent, beyond the repo suite)

Because the verifier is not allowed to edit test files during task-4, the
findings below were reproduced with an ad-hoc deterministic harness
(`target/qa-verify/adhoc.rs`: a copy of the acceptance harness plus the two
adversarial cases, compiled directly against the built rlibs; the source and
binary live in the git-ignored build directory). All three were reported
before the task-5 patch round and re-tested after it.

| # | Scenario | Pre-patch result | Post-patch result |
|---|---|---|---|
| F1 | **Stale retry overwrote newer content.** A→B link down; A copies X (transfer aborts, auto-retry scheduled +2.5 s); link recovers; B copies newer Y and A applies Y; A's retry then re-offers X; B (whose echo marker was cleared by its local copy of Y) applies X → A=Y, B=X divergence at t≈8.2 s (observed `sync_peers` still registered on both sides). | **FAIL** (`stale_retry_does_not_overwrite_content_received_after_the_retry_was_scheduled`) | **PASS** |
| F2 | **Complete transfer queued before a disconnect applied afterwards.** Capture offer + all chunks, disconnect both peers, then deliver the queued datagrams: the receiver applied the snapshot although its sync peer set was empty. | **FAIL** (`a_complete_transfer_queued_before_a_disconnect_is_not_applied_after_it`) | **PASS** |
| F3 | **Pending remote apply vs newer local copy** (static analysis only: the private service loop cannot be driven from an external test without a fake-backend seam): a remote snapshot whose `backend.write()` failed is retried for up to ~4.7 s; a local copy made meanwhile was not invalidating that pending write, so a later successful retry could overwrite the newer local copy. | static finding (no executable repro) | **fixed by inspection + unit test**: `cancel_pending_apply` is called in both local-change branches and on disable; `a_new_local_copy_cancels_a_pending_apply` covers the deferred-commit/`Cancel` behaviour. |

Patch regression tests added by the implementation (all green in the runs
above, 20× repeated through the acceptance harness/unit runs):

- `input-clipboard/src/sync.rs`:
  `stale_retry_does_not_overwrite_content_received_after_the_retry_was_scheduled`,
  `applying_remote_content_cancels_superseded_in_flight_transfers`,
  `a_transfer_queued_before_a_disconnect_is_not_applied_after_it`,
  `a_write_only_peer_can_send_content_but_is_not_offered_ours`;
- `src/clipboard.rs`: `a_new_local_copy_cancels_a_pending_apply`.

Recommendation: fold the two ad-hoc cases into the permanent harness
(`tests/qa_clipboard_atomicity.rs`) after commit, and drop the two `#[ignore]`
attributes — the unit tests above cover the logic, but the end-to-end harness
cases protect the contract over the whole state machine.

## 7. Reproducing the adversarial checks

```sh
# Rebuild the ad-hoc verifier binary against the current rlibs (the two crates
# must come from the same build; try the rlib pairs until rustc accepts them):
for IC in $(ls -t target/debug/deps/libinput_clipboard-*.rlib); do
  for LP in $(ls -t target/debug/deps/liblan_mouse_proto-*.rlib); do
    rustc --edition 2021 --test target/qa-verify/adhoc.rs \
      -L dependency=target/debug/deps \
      --extern input_clipboard="$IC" --extern lan_mouse_proto="$LP" \
      -o target/qa-verify/adhoc 2>/dev/null && break 2
  done
done
target/qa-verify/adhoc stale_retry_does_not_overwrite a_complete_transfer_queued
```

## 8. Residual risks and known limitations

These remain after the fix; none reproduce the reported two-machine symptom in
the deterministic harness, but they are documented so they are not mistaken for
covered cases.

- **R-1 Mixed-version pairs (old ↔ new build).** New sender + legacy receiver:
  the legacy receiver may silently ignore a re-offer of content it already
  applied (its global `last_applied` early-return), so the new sender can burn
  its bounded retry budget before reporting an error even though the content was
  delivered. New receiver + legacy sender: if the local write fails, the new
  receiver cancels after ~4.7 s while the legacy sender's own retry budget is
  shorter; the content may be applied but the legacy sender reports a failure
  and the user may re-copy. Recommendation: run the same release on both
  machines (the wire remains fully compatible for normal operation).
- **R-2 Three or more machines.** Echo suppression is content-hash based and
  unchanged: content most recently received from one peer is still treated as
  "came from a peer" and is not relayed to a third one. The supported/verified
  topology is two machines.
- **R-3 No cross-peer timestamps.** If both sides apply newer content and both
  re-offer around the same time (simultaneous reconnect/backend restart), the
  final state resolves by last writer per side; the bounded retry logic and the
  new supersede rules remove the reproduced divergence windows, but a global
  ordering is not on the wire.
- **R-4 Windows notification cancellation.** `changed()` now retries the read,
  but the future is still recreated on every `select!` iteration in
  `src/clipboard.rs`; in theory a notification can still be cancelled while a
  retry is sleeping. Runtime-only risk; verify on Windows with a clipboard
  manager (Ditto/Windows history) per the manual matrix.
- **R-5 X11 slow/stuck selection owners.** A read that exceeds the 10 s X11
  budget is still dropped and only recovers on the next owner change (same as
  the audit H10); unchanged by this fix.
- **R-6 Portal owner-is-us / slow source app.** Owner-change reads are retried
  when they come back empty, but the GNOME/KDE "we are the owner" stale-read
  quirk and slow source applications still need runtime validation (L5).
- **R-7 Permanent peer-side rejections retry.** An oversized snapshot for the
  peer's limits is rejected with `Cancel`; the sender then auto-retries every
  2.5 s (up to 20 times ≈50 s) before reporting an error. Bounded and
  background-only, but it produces log/CPU churn on permanently mismatched
  configurations; a future improvement is to treat `Cancel` as terminal.
- **R-8 Very large payloads.** Snapshot transfer is still whole-snapshot and
  paced (≤6 chunks per 15 ms tick ≈400 KiB/s); a multi-MiB image delays its text
  representation and may exhaust the retransmission budget on slow links. The
  size caps and the bounded retry surface an error instead of hanging.
- **R-9 Empty/cleared clipboard.** "Clear" is still not propagated (empty
  snapshots are ignored by design); the peer keeps its previous content.
- **R-10 Uncommitted tree.** All results apply to the dirty tree whose diff
  sha256 is pinned in §1. After the lead commits, re-run
  `cargo test --workspace` + the acceptance harness against the commit (the
  verifier did not commit, push, or trigger CI).
- **R-11 Windows/Ubuntu runtime not executed here.** No Windows machine and no
  display/portal in this environment; the platform backends are type-checked
  only. The manual matrix is mandatory for release sign-off.

## 9. Windows 10/11 ↔ Ubuntu manual checklist (post-fix)

The full matrix, backend discovery, exact recipes and sign-off template are in
[clipboard-os-matrix.md](clipboard-os-matrix.md). Minimum rows to execute on
real machines for this fix, in both directions and both host/client roles:

1. **S5** — first copy within ~1 s of the connection coming up: 10/10 trials
   without a second Ctrl+C.
2. **S6/S14** — larger copy + cut the link for 1–3 s mid-transfer: after the
   link returns the paste works without pressing Ctrl+C again (this class was
   `reconnect_after_an_interrupted_transfer_resyncs_the_current_clipboard`).
3. **S3/S7** — 50 rapid copies: the last value always wins, no older paste after
   a newer one.
4. **S12** — disable/enable clipboard sync during/around a copy; current
   clipboard resyncs after re-enable.
5. **S13** — restart the Wayland portal (or the app on Windows) around a copy;
   clipboard resumes, no repeated consent loop.
6. **S16** — old peer build (no clipboard commit behaviour) still exchanges
   pointer/keyboard and reaches a consistent clipboard in normal use.
7. **S17/S18** — write-only/read-only clipboard permissions: only the granted
   direction transfers, control frames never time out (the old blanket gate
   broke this).
8. **S9/S11** — oversized text/image is rejected with a visible
   `clipboard status` error and a later small copy works; PNG/HTML round-trip.
9. **R-4/R-5/R-6 spot checks** — Windows clipboard manager contention, GNOME
   Wayland clipboard sync while the source app is slow, X11 fallback session.

Record per machine: app version/tag, OS build, backend name from
`lan-mouse cli clipboard status`, `detected`/`received` counters, debug logs
(`LAN_MOUSE_LOG_LEVEL=debug`, look for `clipboard: local change detected`,
`clipboard: syncing …`, `clipboard: committed content to the local clipboard`,
`clipboard: re-offering a copy …`, `clipboard: transfer … failed`).
