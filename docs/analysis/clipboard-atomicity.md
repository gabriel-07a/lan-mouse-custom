# Clipboard atomicity audit: why one `Ctrl+C` can be lost and a second one fixes it

**Scope:** read-only audit of the clipboard pipeline in `input-clipboard/`, `lan-mouse-proto/src/clipboard.rs`,
`src/clipboard.rs`, `src/connect.rs`, `src/listen.rs`, `src/emulation.rs` and `src/service.rs` (clipboard wiring
only). No production source was modified. Repo state: `03f2a46` (`chore(release): bump app version to 0.2.17`) +
untracked verifier harness `tests/qa_clipboard_atomicity.rs`; all line numbers below were verified against that
tree.

**Reported symptom:** `Ctrl+C` on one machine is sometimes not pasteable on the other; pressing `Ctrl+C` again
(on the same selection) fixes it. `Windows 10/11 ↔ Ubuntu`, both directions. Required guarantee: a completed
copy that was applied locally must become pasteable on the peer, atomically (per copy, last copy wins), across
connect/disconnect, backend start/restart and ordinary UDP loss.

**Method:** static trace of the copy lifecycle end to end (capture → local change → sync state machine →
transfer → wire → peer receive → apply), plus a review of the existing tests and of the acceptance harness in
`tests/qa_clipboard_atomicity.rs` (owned by the independent verifier). No runtime repro was possible on this
machine; OS-runtime claims are marked as hypotheses needing hardware validation (L5).

---

## TL;DR — ranked hypotheses

| # | Hypothesis | Sev | Symptom fit | Key evidence (file:line) |
|---|---|---|---|---|
| **H1** | **No delivery guarantee: no apply-level acknowledgement, transfers abort after ~1.2 s and the last local snapshot is never retried automatically.** Any transient stall (Wi-Fi roam / DTLS reconnect, peer backend restart, peer event loop busy, MDNS/portal hiccup) loses the copy permanently. A second `Ctrl+C` succeeds because the aborted transfer cleared `last_sent` and a new local change re-arms the pipeline, by which time the transient condition is gone. | **P0** | direct | `input-clipboard/src/transfer.rs:21-23,96-181,184-236`; `input-clipboard/src/sync.rs:265-317,321-334,117-120`; `src/clipboard.rs:389-406`; `lan-mouse-proto/src/clipboard.rs:60-70` |
| **H2** | **The local change can be consumed by the backend without ever being offered** (async backend startup/restart gap, echo/duplicate filters and swallowed read errors), and a later peer registration only re-reads the clipboard for *outgoing* clients — peers known only through an incoming connection are registered without any re-read. A second copy works because the second OS change event is the first thing that actually reaches the sync layer / finds a registered peer. | **P0/P1** | direct | `src/clipboard.rs:133-241,349-374,408-428,439-451`; `input-clipboard/src/windows.rs:57-59,104-116,247-264`; `input-clipboard/src/portal.rs:185-208`; `input-clipboard/src/wayland.rs:191`; `input-clipboard/src/x11.rs:64,375-386` |
| **H3** | **Sync state is global and per-sync-instance: hash dedupe (`last_sent`/`last_applied`), peer identity churn and serial reset suppress retries and late/re-registered peers.** The retry is swallowed or the retried transfer is fake-acked; a second copy eventually gets a new serial/clean state and works. | **P1** | direct (recopy path), plus "stuck until different copy" variants | `input-clipboard/src/sync.rs:113-127,158-181,69-72,250-257`; `src/clipboard.rs:336-369`; `input-clipboard/src/transfer.rs:184-236` |
| **H4** | **One-way clipboard permissions are mis-gated:** `Ack`/`Cancel` frames we send are gated by the peer's *read* capability instead of the *write* capability of the direction they serve, so a write-only grant (peer may set our clipboard) can never complete. | **P1** | config-dependent, not "sometimes" | `src/clipboard.rs:48-83` vs `src/listen.rs:890-899`; `lan-mouse-ipc/src/lib.rs:419-448` |
| **H5** | **Whole-snapshot atomicity is too coarse for the transfer budget:** one slow/oversized representation aborts/withholds the entire snapshot (text included), and the 1.2 s abort window is measured from the first window. Large or multi-format copies (browser screenshot: text+html+png) are the worst case. | **P2** | copy-type-dependent | `input-clipboard/src/transfer.rs:269-303,386-390`; `input-clipboard/src/lib.rs:117-134`; `input-clipboard/src/sync.rs:103-111,265-317`; `src/clipboard.rs:389-400` |
| **H6** | **Clear/empty/secret clipboards are silently dropped:** the peer keeps stale content; the user reads it as "the copy didn't arrive" and re-copies. | **P3** | partial | `input-clipboard/src/sync.rs:104-106`; `input-clipboard/src/windows.rs:152-155`; `input-clipboard/src/portal.rs:196-200`; `input-clipboard/src/wayland.rs:110-121` |
| **H7** | **Windows echo suppression race:** a genuine user copy that lands between our remote-write and `GetClipboardSequenceNumber()` is mistaken for our own write; a copy that gets the clipboard notification while another app holds the clipboard is dropped (read error is only logged). | **P2** | direct, Windows source | `input-clipboard/src/windows.rs:57-59,104-116,247-264,386-401`; `src/clipboard.rs:426` |
| **H8** | **Change-event loss from `select!` cancellation:** `backend.changed()` is re-created every loop iteration; if the WM_CLIPBOARDUPDATE notifications were already dequeued and the branch is cancelled while `read()` awaits (clipboard contention → 20 ms retry sleeps), the notification is lost with no retry. | **P3** | direct, Windows source with clipboard managers | `src/clipboard.rs:408-428`; `input-clipboard/src/windows.rs:104-116,251-264` |
| **H9** | **Portal "we are the owner" quirk:** after a remote apply the portal can keep reporting `session_is_owner == true` for later user copies (already known to the code); the re-read can return our stale snapshot, which the hash check then drops — the user's copy is never forwarded until the flag/state settles. | **P2** | direct, Ubuntu source (GNOME 50+/KDE) | `input-clipboard/src/portal.rs:185-209,258-265,289-294` |
| **H10** | **X11 fallback/known-slow-owner gaps:** without XFixes a second copy from the same owner is invisible; a read that takes >10 s or fails is logged and discarded, not retried. INCR/Oversize paths are bounded but a single failed representation drops the snapshot. | **P2** | Ubuntu X11 source / XWayland fallback | `input-clipboard/src/x11.rs:54-64,187-224,332-390,428-451` |
| **H11** | **Coalescing/debounce is *not* a loss source on the source side** (multiple owner changes collapse to one offer — desired); it only interacts with H3 when the same content is copied twice. Listed here because it was a named suspicion and it was checked. | — | non-cause | `input-clipboard/src/sync.rs:38-44,117-120` |
| **H12** | **UDP loss itself is tolerated for content** (chunk retransmission, offer retransmission, ack loss tolerated by retry) — the loss that hurts is *time*, because there is a hard abort horizon and no post-abort retry (see H1). | — | aggravating | `input-clipboard/src/transfer.rs:1-8,96-181,443-585`; `input-clipboard/src/sync.rs:489-495` |

**Recommended order for task-2 (implementation):** H1 → H2 → H3 (they share the same state-machine
refactor) → H4 (small independent fix) → H5/H6/H9/H10 (platform polish). See §8 for the suggested design and
capability/wire-compatibility constraints.

---

## 1. The copy lifecycle and where it can break

For one copy of snapshot `S` from machine **A** to machine **B** (A = source, B = sink; either machine can be
the OS "host" or "client" — the transport is one bidirectional DTLS association and clipboard roles are
independent of mouse-control roles):

| Stage | Code path | Failure modes observed in code |
|---|---|---|
| S1. OS observes the local copy | backend watcher (`WindowsClipboard::changed` `windows.rs:251-264`; portal task `portal.rs:154-232`; Wayland watcher thread `wayland.rs:88-137`; X11 thread `x11.rs:153-391`) | change event consumed but read fails/empty/secret (H2/H7/H8/H10); backend not running yet (startup/restart, H2); same-owner copy invisible without XFixes (H10). |
| S2. Service loop receives the snapshot | `src/clipboard.rs:408-428` (`change = backend.changed()`), then `sync.on_local_change(snapshot, now)` (`:419`) | `Err` is only logged (`:426`), no retry; disabled toggle drops intentionally (`:418`). |
| S3. Sync decides to send | `input-clipboard/src/sync.rs:103-156` (`on_local_change`) | empty / oversize rejected whole (`:104-111`); echo filter `last_applied` (`:113-116`); duplicate filter `last_sent` (`:117-120`); **no peers → dropped and not recorded** (`:121-126`); sets `last_sent` before sending (`:127`). |
| S4. Transfer starts | `OutgoingTransfer::new` + first `poll` (`sync.rs:142-149`, `transfer.rs:64-181`) | previous in-flight transfer is cancelled (`sync.rs:133-140`); offer+chunks are pushed to a local unbounded channel (`src/clipboard.rs:72-83,276-293`) — send errors in `send_raw` are only logged (`src/clipboard.rs:284-286`). |
| S5. Wire | DTLS/UDP frames 12–15, chunked at 1024 B, cumulative ACKs every ≤10 ms/16 chunks (`lan-mouse-proto/src/clipboard.rs:16-70,86-238`; `transfer.rs:27-31,326-384`) | loss tolerated while retries make progress; **hard abort after 10 windows without progress ≈ 1.2 s** (`transfer.rs:21-23,168-179`). |
| S6. Peer receives frames | listener/connect receive loops (`src/listen.rs:861-912`, `src/connect.rs:1294-1316`) → emulation task forwards (`src/emulation.rs:642-647`) → clipboard task channels (`src/clipboard.rs:429-462`) | policy gate drops frames (`listen.rs:890-899`); frames for an unregistered service peer get a synthetic peer id or are ignored while routes/peer ids churn (H3); channel is unbounded, so frames queued during a backend outage are processed late and stale (H2). |
| S7. Receiver assembles | `sync.rs:158-226` (`handle_frame`), `transfer.rs:269-441` (`IncomingTransfer`) | offer rejected whole on any oversize (`transfer.rs:281-303`); duplicate-serial offer path re-sends stale ACKs and ignores the new offer (`sync.rs:174-181`, H3); transfer only returned one per tick (`sync.rs:303-317`) after being removed from the map. |
| S8. Receiver applies to the OS clipboard | `sync.tick` returns the snapshot → `src/clipboard.rs:389-400` → `Clipboard::write` (per backend) → `sync.on_applied` (`sync.rs:321-334`) | **on write error the snapshot is gone** (already removed from `incoming`, `sync.rs:303-316`); sender is not told (its ACKs were only for chunks) and keeps `last_sent`; `on_applied` is not reached, so `last_applied` is not updated. |
| S9. Peer local paste | OS clipboard content | If S8 silently failed, paste yields the old content; the sync layer believes the transfer succeeded. |

The missing edge is **S8 → A**: there is no end-to-end "applied" confirmation. `Ack` acknowledges chunk
arrival only (`lan-mouse-proto/src/clipboard.rs:60-70`), and the sender's "done" condition is purely
chunk-based (`transfer.rs:238-244`). That is the architectural gap behind H1/H3.

---

## 2. Hypothesis details (why a second copy can succeed)

### H1 — No apply-level ack; hard abort horizon; no automatic retry (P0)

**Mechanism.**
1. `OutgoingTransfer::poll` counts a *window* attempt every time a full window was emitted and then waits
   `ACK_TIMEOUT = 120 ms`; after `MAX_WINDOW_ATTEMPTS = 10` without progress the transfer is marked failed
   (`transfer.rs:21-23,108-180`). For small text this is ≈ **1.2–1.5 s** from the first offer (the initial
   sweep can add up to ~90 ms because only `MAX_CHUNKS_PER_BURST = 6` chunks are emitted per 15 ms service
   tick; `transfer.rs:26,143-159`, `src/clipboard.rs:307`).
2. `ClipboardSync::tick` turns failure into `last_sent = None` + one `Cancel` to the peer + a status error
   (`sync.rs:265-289`). **It does not re-offer the snapshot.** After the abort the only ways to send content
   again are (a) a new local change event, (b) a newly registered *outgoing* client (registration read,
   `src/clipboard.rs:349-374`), or (c) disable/enable (`sync.rs:87-93`).
3. The receiver never tells the sender whether S8 succeeded; a completed transfer whose `write()` failed is
   dropped silently (`sync.rs:303-316`, `src/clipboard.rs:389-400`). The sender's transfer is already gone
   from `outgoing` (all chunks ACKed) and `last_sent` is still set.

**Why the transient triggers are realistic:** the receiving task's loop can be unavailable or slow exactly
when the copy lands: backend (re)creation after a portal session restart (`RunExit::BackendStopped`,
`src/clipboard.rs:230-237`; startup creation is inside the task before `run`, `:201-214`), a long
`backend.write()`/`backend.changed()` await on the receiver (`PORTAL` selection budget 4 s, `portal.rs:36-40`;
X11 INCR up to 10 s, `x11.rs:64`), Wi-Fi roaming / DTLS reconnect (frames are lost for the roam duration,
connection state churn `connect.rs:1000-1030`), or portal/consent weirdness at start. The 1.2 s budget is much
shorter than these windows, so the first copy after such an event is dropped and no retry follows.

**Why the second copy succeeds:** by then the condition has passed; the new local change event calls
`on_local_change`, the abort cleared `last_sent` (`sync.rs:279-283`), and the same bytes are transferred
successfully. This is the most direct explanation of the reported symptom and it is trigger-agnostic (covers
peer startup, reconnect, UDP burst loss, receiver restart).

**Evidence anchors:** `transfer.rs:21-23,96-181,184-236,238-244`; `sync.rs:265-289,303-317,321-334,117-120`;
`src/clipboard.rs:201-241,389-406`; `lan-mouse-proto/src/clipboard.rs:60-70`.

**Deterministic repro (current code, expected red):** `ClipboardSync` harness, drop every ACK for
`> MAX_WINDOW_ATTEMPTS` simulated windows (≈ 1.3 s of `now += 10 ms` steps) so the transfer fails; then stop
dropping and run 100 more steps **without a new local copy**. Current result: nothing is re-sent (peer stays
stale). Expected after fix: the pending snapshot is re-offered with backoff and the peer applies it.

### H2 — The change event can be consumed and never offered; registration recovery is one-sided (P0/P1)

**Mechanism.** Several paths consume a local change without a corresponding offer, and nothing re-reads the
clipboard afterwards:

1. **Backend startup/restart gap (all OSes).** `ClipboardTask::new` creates the backend *before* `run` starts
   (`src/clipboard.rs:184-241`), and a stopped backend is recreated only after the old `run` returned
   (`:201-238`). Frames are queued in the unbounded local channels while the backend is unavailable
   (`local_channel` is unbounded; `src/clipboard.rs:100-104,251-253`), and `run` then processes stale frames
   (`from_listener`/`from_connection`, `:429-462`). Copies made *by this machine* during the gap are not
   observed at all (no watcher running), and a queued peer frame can be stale by the time it is handled.
2. **Only outgoing clients get a "push the current clipboard" step at registration.**
   `src/clipboard.rs:349-374` calls `backend.read()` + `sync.on_local_change(...)` when an *outgoing* client
   handle becomes connected. A peer that exists only because it dialed us is created lazily on the first
   clipboard frame (`src/clipboard.rs:439-451`) and **never triggers a read/re-offer**. So on a link whose
   reverse direction never sent clipboard data yet (one-way config, failed reverse dial, auto-reverse client
   without a known port), a copy made on the listener side is dropped at `sync.rs:121-126` (no peers) and is
   still lost when the peer later appears — unless the user copies again.
3. **Backend echo/duplicate filters and swallowed read errors** (platform-specific, detail in H7/H9/H10):
   `changed()` may return nothing usable while the notification was already consumed. The sync layer only
   sees `Some(snapshot)`/`None`/`Err`; `Err` is logged and dropped (`src/clipboard.rs:426`), there is no
   pending re-read.
4. **Backend restart drops incoming-route peers entirely:** a new `run` starts a fresh `ClipboardSync` with an
   empty peer set (`src/clipboard.rs:301`), `known_outgoing`/`addr_peers` reset (`:302-304`); outgoing clients
   re-register via the tick loop, but incoming-only peers are only re-added when their next clipboard frame
   arrives (`:439-451`). Copies on our side in that window are accepted locally but not offered.

**Why a second copy succeeds:** a new OS notification re-runs S1–S3; by then peers are registered (or the
backend is up), so the offer is actually sent. The first copy had no automatic recovery path.

**Evidence anchors:** `src/clipboard.rs:100-104,108-126,133-241,251-253,336-387,408-428,429-462`;
`src/emulation.rs:642-647`; `input-clipboard/src/lib.rs:212-229` (backend trait semantics).

**Deterministic repro (current code, expected red):** service-glue harness (`src/clipboard.rs` tests / the
independent `tests/qa_clipboard_atomicity.rs` Node): (a) connect a peer as *incoming* only, set the local
clipboard, run ticks → expect an Offer (today: none); (b) make the receiver's backend "not ready" for 2 s
(a fake backend whose `changed()` is pending and `write()` fails), copy, then let it become ready → expect the
snapshot to be delivered without a second copy (today: sender aborted and nothing is retried).

### H3 — Global hash dedupe, per-sync serial and peer-id churn (P1)

**Mechanism.**
1. **`last_sent` is global and never reconciled with the peer set.** It is set before sending
   (`sync.rs:127`) and cleared only on `on_applied` (`:321-334`), transfer failure (`:279-283`) or `reset`
   (`:87-93`). Consequences: (a) a new/re-registered peer never receives the current clipboard if the same
   content was already offered to a different peer (`src/clipboard.rs:372-374` + `sync.rs:117-120`);
   (b) after a disconnect mid-transfer (`sync.remove_peer` drops `outgoing` without clearing `last_sent`,
   `sync.rs:69-72`), the reconnect registration push is suppressed — the peer is stuck until different content
   is copied (this is the "recopy does not help" variant); (c) a copy repeated after a *successful-looking*
   transfer whose apply failed is suppressed forever (H1/S8).
2. **`last_applied` echo filter returns without any ACK.** `handle_frame Offer` with
   `last_applied == hash` returns immediately (`sync.rs:169-171`); the sender is left without ACKs and retries
   until the abort horizon, then clears `last_sent`. This burns the copy for ~1.2 s and is another path where
   only a later third copy (after the sender's failure clears state) can work.
3. **Serial space resets with the sync instance.** `next_serial` starts at 0 in a new `ClipboardSync`
   (`sync.rs:49-63,129-130`), and a backend restart creates a new instance (`src/clipboard.rs:230-237`).
   If the peer still holds an `incoming` transfer for our peer id with the same serial (incomplete, up to
   `TRANSFER_TIMEOUT = 30 s`, `transfer.rs:31,421-424`), it treats the new `Offer` as a retransmission of the
   old transfer: it **re-sends old ACKs instead of accepting the new content** (`sync.rs:174-181`). The new
   sender can consume those ACKs (`transfer.rs:184-236` checks serial + mime + cumulative base, but the base
   refers to a different snapshot) and finish without the receiver ever applying the new data. A second copy
   gets serial 2, does not match, and works. This is a rare but exact "first copy lost, second fixes" path
   tied to backend restarts/reconnects.
4. **Peer identity churn tears down in-flight transfers.** When an outgoing client handle becomes known, the
   service merges a previously-created incoming-address peer into the handle
   (`src/clipboard.rs:349-364`): it removes the route and calls `remove_peer(stale)`, but `remove_peer` only
   clears `peers`/`outgoing`, not `incoming` (`sync.rs:69-72`). Frames arriving after the merge are delivered
   under the new peer id, so `incoming.get_mut(&peer)` misses and the transfer stalls (chunks ignored, no
   ACKs); the stale incoming entry then lingers until the 30 s timeout. The sender aborts after ~1.2 s and
   clears `last_sent`; a copy after the merge works. This is reachable on every link where the peer was first
   seen through the listener and the reverse/outgoing registration completes while a copy is in flight — i.e.
   exactly around connection/handoff time.

**Evidence anchors:** `sync.rs:49-72,113-127,158-181,250-257,265-289,321-334`;
`src/clipboard.rs:301-369,439-451`; `transfer.rs:184-236,421-424`.

**Deterministic repro (expected red on current code):**
- `newly_registered_peer_receives_the_current_snapshot`: peer 2 registered, copy X offered to 2
  (`last_sent = H(X)`), then peer 3 registers with `resync`; expect `Offer` to 3 (today: suppressed).
- `reconnect_after_interrupted_transfer_resends_the_same_content`: copy X, deliver offer+chunks partly, remove
  peer, re-add + resync; expect re-offer (today: `last_sent` blocks it).
- `stale_incoming_transfer_does_not_ack_a_new_serial_epoch`: receiver holds incomplete incoming for serial 1
  from origin O1; sender restarts (new origin/epoch) and offers serial 1 with different hash; expect the old
  transfer to be discarded and the new content applied (today: old ACKs fake progress / new offer ignored).

### H4 — One-way clipboard capability mis-gated on ACK/Cancel (P1)

`Sink::peer_may_read` gates **every** outbound frame, including ACKs and Cancels, on the peer's
`incoming.allows_clipboard_read()` (`src/clipboard.rs:48-83`). But ACK/Cancel are *control frames* for an
inbound transfer: they are needed for the peer→us direction, which is authorized by
`allows_clipboard_write()` (and that is exactly the check the listener uses for inbound frames,
`src/listen.rs:890-899`). With a write-only grant (`clipboard-write` = "peer may set our clipboard"):

- inbound Offer/Chunk are accepted by the listener (write capability ✔) and `sync` accepts them,
- our ACKs back to the sender are dropped by `Sink::send` (read capability ✘),
- the sender aborts after ~1.2 s, exactly as in H1; the user sees "the other machine's copy doesn't arrive".

This is a persistent failure for a symmetric-looking configuration and is worth fixing independently of the
atomicity refactor: classify frames (content vs control) and gate content frames on read, control frames on
write (or no gate for IDs strictly belonging to a transfer we are receiving). Also note the policy is
evaluated per *route* (`Route::Outgoing`/`Route::Incoming`, `src/clipboard.rs:33-38,72-83`): for the incoming
route an unknown fingerprint stays permissive (`:55-69`), so behavior can change as soon as the fingerprint is
learned; match this in tests.

### H5 — Snapshot granularity vs. delivery budget (P2)

`Limits::accepts` rejects the whole snapshot if *any* representation is oversized (`lib.rs:117-134`), the
receiver rejects the whole offer likewise (`transfer.rs:281-303`), and the receiver only assembles/applies
when **all** representations are complete (`transfer.rs:386-390`; `sync.rs:303-317`). A browser copy of an
image usually carries `text/plain` + `text/html` + `image/png`; at `MAX_CHUNKS_PER_BURST = 6` per 15 ms tick
the paced throughput is ≈ 400 chunks/s ≈ 400 KiB/s, i.e. an 8 MiB PNG takes ≈ 20 s while the text is already
on the peer but not applied (and the receiver's apply is withheld until the last chunk). If the image then
fails (loss burst, sender abort, receiver limit), the text is lost too, and the user's "copy again" often
copies only text or a smaller payload → works. Fix direction: per-representation application or at least
apply the text/HTML portion when it is complete and the image is still streaming (or deliver "text fast,
image later"), and make the abort horizon an idle timeout rather than a fixed attempt count.

### H6 — Clear / empty / secret clipboards are not propagated (P3, expected behavior but confusing)

`on_local_change` returns on `snapshot.is_empty()` (`sync.rs:104-106`); Windows marks secret clipboards as
empty (`windows.rs:152-155`), the portal skips secret MIME hints (`portal.rs:196-200`), Wayland skips secret
hints (`wayland.rs:110-121`). Consequence: clearing the clipboard is not propagated (the peer keeps stale
content), and a password-manager copy intentionally does not sync. Users report this as "Ctrl+C did nothing",
then copy something else and it works. Decide explicitly for task-2 whether a "clear" frame should be sent
(and whether it is safe cross-OS); document the secret behavior in the user-facing docs.

### H7 — Windows source: echo filter races and dropped read errors (P2)

- `write()` publishes the remote snapshot, then stores `own_sequence = GetClipboardSequenceNumber()`
  (`windows.rs:213-249`, set at `:247`). `changed()` ignores notifications while the current sequence equals
  `own_sequence` (`:251-264`). A genuine user copy racing the apply can be swallowed: sequence is taken
  *after* the write, so any change that happens in between makes the user's sequence the "own" one. The
  window is small but this is precisely the "sometimes" class.
- `changed()` returns `Err` when `open()` fails after 10 × 20 ms retries (`windows.rs:104-116`); the service
  logs it and **consumes the notification** (`src/clipboard.rs:426`). Clipboard managers, Office, RDP or UWP
  clipboard flush can hold the clipboard long enough; the copy is lost, the next one usually works.
- `read()` returns an empty snapshot when `is_secret()` is true (`windows.rs:152-155`), which is silently
  ignored by the sync layer (H6).

Fix direction: capture the sequence *before* `EmptyClipboard` and treat only a bounded own-write sequence
range as echo; on `open()` failure schedule a bounded retry for the *content that was current at notification
time* (e.g. re-read after 50-200 ms) instead of dropping; move the echo decision into a pure helper so it can
be unit-tested on Linux (`#[cfg(windows)]` code can't run in CI).

### H8 — Windows source: notification loss via `select!` cancellation (P3)

`run` recreates `backend.changed()` on every loop iteration (`src/clipboard.rs:408`). `tokio::select!` drops
the non-winning futures when another branch fires (the 15 ms ticker, peer frames, listener commands). On
Windows, `changed()` first dequeues a `()` from the notification channel and then awaits `read()`; if
`read()` awaits (open retries sleep 20 ms on clipboard contention), a ticker/peer branch can win and cancel
the already-dequeued notification. No retry exists. Mitigation: consume notifications in a dedicated
`BackendEvent` stream/state machine that is not cancelled mid-read, or keep an internal "pending re-read"
flag set before awaiting.

### H9 — Ubuntu portal: owner-is-us re-read can return stale content (P2)

`PortalClipboard` intentionally does not skip owner events where `event.session_is_owner() == Some(true)`
(comment says GNOME can keep the flag true for later user copies — `portal.rs:185-209`). It then calls
`read_selection(...)` with the event's MIME list. If the portal still routes the read to our own session, the
snapshot read back is our previous remote-applied content; the sync layer drops it via `last_applied`
(`sync.rs:169-171`) and **the user's new copy is never offered**. The next copy (when the flag is correct or
the owner is unambiguous) works. This matches the Ubuntu-source side of the report and the code comment that
already documents the quirk. Needs L5 validation (GNOME 50/KDE) because the portal semantics are runtime-only;
fix direction: when the event claims we are the owner but the MIME list/content hash differs from what we
applied, force a fresh read *after* a short defer and/or use `selection_read` only when the owner is not us.

Also note the portal's read budget: per-MIME 2 s, whole selection 4 s (`portal.rs:36-40,331-375`); a slow
source app (browser serving a large selection) makes a copy silently fail (debug log), next attempt works.
Additionally the portal event loop serves incoming *transfer* requests in the same `select!` as owner-change
reads (`portal.rs:183-231`): a long read (up to 4 s) delays answering a local paste for that long, and the
app may time out its paste.

### H10 — Ubuntu X11 source: XFixes requirement and silent read timeouts (P2)

`x11.rs` relies on XFixes `SET_SELECTION_OWNER` to see a second copy from the same application
(`x11.rs:187-203,337-350`); on a server without XFixes only owner-id *polling* exists and same-owner copies
are invisible (`:355-373`). A read that gets no `SelectionNotify` is silently reset after 10 s
(`:375-379`) with no retry; `start_read` replaces an in-flight read when a newer owner change arrives (`:345-348`)
— intended (last copy wins) but means the first content is not delivered if the second fails. Also X11 is
the fallback when GNOME < 50 Wayland has no clipboard portal and no wlr-data-control (`lib.rs:358-398`,
`portal.rs:3-8`, `wayland.rs:3-5`); through XWayland this usually works (Mutter bridges selections) but is
environment-dependent — worth a documented check (`doctor` prints the backend name; see §9).

---

## 3. Why a second copy can succeed while the first is lost

Consolidated answer to the task's explicit question:

1. **The failure state is transient; recovery is user-triggered only.** Transfer abort clears `last_sent`
   (H1, `sync.rs:279-283`) and the only automatic re-offer is registration read (outgoing peers only, H2).
   The second copy is the first retriggering event after the transient condition (Wi-Fi roam, reconnect,
   peer backend restart) has passed.
2. **The first change event was consumed by the OS layer without a usable snapshot** (read error, secret
   empty, X11 timeout, Windows clipboard contention, portal stale-owner read: H7/H8/H9/H10). Only a new OS
   notification produces a new snapshot; the second copy is that notification.
3. **The first offer was suppressed or fake-completed by sync state** (global `last_sent`/`last_applied`,
   stale incoming with a reused serial, dedupe after a failed apply: H1/H3). The second copy gets a new
   serial/state and the state machine starts cleanly.
4. **The peer/route set changed between the two copies** (peer not registered yet for outgoing capability
   negotiation, incoming-only peer created lazily, duplicate-peer merge mid-transfer: H2/H3). The second copy
   happens after registration/merge has settled.
5. **The second copy often differs in content/formats** (user re-selects, or the app emits a different format
   set on the second `Ctrl+C` — e.g. text first, then text+html): a different hash bypasses the hash-based
   dedupe (`sync.rs:112-127`) and can also produce a smaller snapshot that avoids the size/transfer-abort
   paths (H5).

None of these is a deterministic "copy N always fails" bug; they are all *missing recovery/confirmation*
paths, which is exactly what an atomicity fix must add.

---

## 4. Cross-OS / direction matrix (clipboard only)

Legend: **✓** expected reliable; **⚠** known fragile path (hypothesis); **✗** not supported/never works in that
configuration; `S` = source (where `Ctrl+C` happens), `D` = destination. "Role" = whether the machine was
configured as the other's client (outgoing dialer) or is reached as a listener. Clipboard data flows both ways
on one DTLS association; the role only changes routing/registration details (H2/H3), not the frame format.

| Source (S) ↓ / Destination (D) → | Windows 10/11 | Ubuntu GNOME 50+ / KDE (portal) | Ubuntu wlroots (data-control) | Ubuntu X11 / XWayland fallback |
|---|---|---|---|---|
| **Windows 10/11** | ✓ text/HTML/PNG; ⚠ H7 (sequence race, clipboard contention), ⚠ H8 (notification loss) on S; D apply is Windows `write` (can fail transiently → H1) | ✓ D portal apply can be async/slow; ⚠ H1/H9 on D; large PNG waits for full snapshot (H5) | ✓ D `copy_multi` child process; ⚠ H1/H5 on D | ✓ D X11 selection/INCR; ⚠ H10 read-back and INCR validation on D |
| **Ubuntu GNOME 50+ / KDE (portal)** | ✓ S portal read budget/slow owner → ⚠ H9, H5; D Windows apply ⚠ H1/H7 | ✓ both sides portal; ⚠ H9 both sides; H1 on D | ⚠ source portal, dest data-control; H1/H9 | ⚠ source portal; dest X11 H10 |
| **Ubuntu wlroots (data-control)** | ✓ S watcher thread blocking reads (H2/H10-class); D Windows H1/H7 | ✓ S data-control, D portal H9/H1 | ✓ ⚠ H2/H10-class on S (watcher thread dies → restart path) | ⚠ H10 |
| **Ubuntu X11 / XWayland fallback** | ✓ S X11 (needs XFixes for same-owner recopy; H10), D Windows H1/H7 | ✓ S X11, D portal H9 | ⚠ cross-backend; H10 | ✓ S/D X11; ⚠ H10, INCR paths compile-only validated |
| **GNOME < 50 Wayland, no portal, no data-control** | ✗/⚠ only via XWayland bridging (`lib.rs:358-398`); plain native Wayland apps invisible → "Ctrl+C never syncs" | — | — | — |

**Direction / role notes (apply to every cell):**

- **Outgoing-configured side** gets the automatic "read current clipboard and offer it" at registration
  (`src/clipboard.rs:349-374`) → self-heals copies missed while disconnected.
- **Incoming-only side** registers a synthetic peer on the first clipboard frame only and never performs that
  read (`src/clipboard.rs:439-451`) → H2: first copy on that side is lost until the peer (or the user) sends
  something.
- **One-way permission** (`clipboard-read` only or `clipboard-write` only) → H4: write-only direction cannot
  complete at all; read-only/symmetric usually fine (both bits set).
- **Both configured** (typical): both sides have outgoing registration; H3 (global dedupe/peer merge) is the
  main exposure.
- **Windows source** additionally has H7/H8; **Ubuntu portal source** H9/H5; **Ubuntu X11 source** H10.
- **Destination side** shares H1 (apply ack missing) and H2 (backend restart/registration) on every OS.

Manual validation matrix (L5) is in §7.3.

---

## 5. Non-causes (verified during the audit)

These were named as suspicions but the code already handles them; they should not be re-fixed:

- **Chunk/MTU framing:** `CHUNK_SIZE = 1024`, `MAX_CLIPBOARD_FRAME = 16 + 1024` (`lan-mouse-proto/src/clipboard.rs:16-29`);
  frames fit IPv4/IPv6 minimum MTUs; malformed/truncated frames are rejected without panics (codec tests,
  `arbitrary_frames_never_panic`, and hostile-frame tests in `sync.rs:685-743`).
- **UDP loss for content:** chunk windows are retransmitted until ACKed; offer is re-sent until the first
  chunk is ACKed; duplicate ACKs/offers are tolerated (`transfer.rs:96-236,326-384`; tests
  `survives_chunk_and_ack_loss` `sync.rs:489-495`, `transfers_with_heavy_chunk_loss` `transfer.rs:563-573`).
  What is *not* covered is time (`H1`) and control-frame loss (H4).
- **Local echo loops:** echo is suppressed by content hash on both sides (`last_applied`, `sync.rs:113-116,169-171`)
  and per-backend own-write filters (`windows.rs:247-264`, `wayland.rs:123-125`, `x11.rs:332-336`,
  `portal.rs:185-209`); test `applied_remote_snapshot_is_not_echoed_back` (`sync.rs:497-507`). Note the
  `last_applied` filter is too aggressive in one direction (returns without ACK, H3.2), but it is not a loop.
- **Coalescing/debounce:** one copy really does emit several owner changes (one per MIME update); the sync
  layer intentionally offers the same content once (`sync.rs:38-44,117-120`, test
  `duplicate_change_events_are_offered_once` `sync.rs:644-653`). This is not a loss source; it interacts with
  H3 only when the user *re-copies the same bytes*.
- **Wire compatibility:** frames 12–15 are outside `EventType`; both receive loops skip undecodable
  datagrams instead of closing the connection (`src/listen.rs:913-941`, `src/connect.rs:1400-1417`), so an
  additive, capability-gated frame type (needed for H1's apply-ack) is safe. `CLIPBOARD` capability already
  gates whether anything is offered (`src/clipboard.rs:345-348`).
- **Clear/empty clipboard:** losing the clear is by design today (`sync.rs:104-106`); decide explicitly (H6),
  do not treat as a regression.

---

## 6. Current test coverage inventory (clipboard)

- `input-clipboard/src/sync.rs::tests` — happy path, recopy after remote apply, chunk+ack loss, echo,
  oversized, transfer supersede, multi-peer; `dedupe_tests` — duplicate events, no-peer pending copy, reset,
  hostile frames, failed-transfer re-offer.
- `input-clipboard/src/transfer.rs::tests` — lossy transfer, abort when nothing ACKed, hostile ACK bases,
  offer limits, malformed chunks, hash mismatch, pacing.
- `input-clipboard/src/lib.rs::tests` — hash/limit/wire items, memory backend change semantics.
- `src/clipboard.rs::tests` — one DTLS loopback test (offer → listener → ACK back,
  `clipboard_frames_traverse_dtls_in_both_directions`, `src/clipboard.rs:546-634`); no service-glue tests for
  registration/restart/peer churn.
- `lan-mouse-proto/src/clipboard.rs::tests` — codec/fuzz-style roundtrip.
- `tests/qa_clipboard_atomicity.rs` (untracked, owned by the independent verifier) — in-memory service-glue
  harness with deterministic clock/loss; several acceptance tests are deliberately red until the task-2 fix.

**Coverage gaps that let H1–H4 regress freely:** (a) no test of the `ClipboardTask` wiring (registration,
backend restart, disabled/enabled, incoming-peer creation); (b) no test of apply failure at the destination;
(c) no test of per-peer/new-peer delivery; (d) no test of the permission gate for ACK/Cancel; (e) no test of
serial epoch/backend-restart collisions; (f) no Windows echo-filter unit test (needs a pure helper); (g) no
test that a transfer timeout leaves a *retry* pending.

**Cross-reference to the independent acceptance harness** (`tests/qa_clipboard_atomicity.rs`, task-3; untracked
at the time of this audit, so the list may still grow). The following tests already encode parts of the
reported symptom and should be treated as acceptance criteria for task-2 (expected red before the fix, green
after): `copy_before_peer_registration_is_offered_on_connect`,
`copy_missed_by_a_starting_backend_is_offered_on_connect`,
`first_copy_after_connect_survives_a_lost_offer`, `copy_while_transport_briefly_fails_is_retried`,
`pressing_copy_again_after_a_failed_transfer_repairs_delivery`,
`copy_during_a_long_transport_outage_is_recovered_without_a_second_copy`,
`reconnect_after_an_interrupted_transfer_resyncs_the_current_clipboard`,
`copying_again_after_reconnect_is_offered_again`,
`clipboard_survives_a_backend_restart_and_resyncs`,
`disabling_mid_transfer_clears_stale_state_and_re_enabling_resyncs`,
`superseded_transfer_never_overwrites_a_newer_copy`,
`frames_queued_before_a_disconnect_are_not_applied_after_it`. That harness mirrors the *service glue* in
memory; §7.3's service-level tests are still needed because it cannot exercise `src/clipboard.rs` itself
(real registration/route merge, backend factory, policy gate, `select!` loop) and §7.1's pure tests are needed
for the state-machine invariants it does not assert (per-peer commitment, same-content retry after apply
failure, serial epoch).

---

## 7. Concrete deterministic regression tests

Rules: no sleeps (simulated `Instant`, `now += STEP`), in-memory wire, production codec. L0 = pure state
machine, L1 = property, L2 = component with fake backends, L3 = service glue in memory, L4 = serial DTLS
smoke, L5 = hardware/manual.

### 7.1 `input-clipboard` (L0) — most of the guarantee lives here

Add to `input-clipboard/src/sync.rs::tests` (harness style already present, `:337-444`; replay frames per
`STEP`):

| Test name (proposed) | Arrangement / action | Assertion | Today |
|---|---|---|---|
| `copy_with_no_ack_is_retried_until_the_peer_applies` | copy; drop all ACKs for `MAX_WINDOW_ATTEMPTS+2` windows of `poll`; stop dropping; advance 200 steps **without a new local copy** | peer applies the snapshot; no second local change needed | **red** (H1) |
| `apply_failure_on_the_peer_does_not_leave_a_stale_offer` | peer receives complete snapshot but its `apply` fails (harness callback); sender must retry / receive explicit failure and re-offer after the retry window | peer eventually applies; sender does not keep `last_sent` as if succeeded | **red** (H1/S8) |
| `newly_registered_peer_is_offered_the_current_snapshot` | copy X with peer 2 only; register peer 3 + resync; inspect peer-3 frames | Offer/chunks for X go to peer 3 | **red** (H3.1) |
| `reconnect_after_interrupted_transfer_resends_the_same_content` | start copy, deliver half, `remove_peer`, `add_peer` + resync | full re-offer, peer applies | **red** (H3.1) |
| `receiver_rejecting_an_offer_allows_the_same_content_to_be_retried` | receiver `Cancel` (oversize at its limits); sender copies same content again after cancel | new offer is sent (no global `last_sent` block) | **red** (H3.1) |
| `incoming_transfer_survives_peer_identity_migration` | start incoming transfer under addr peer P, deliver half; migrate P→handle H (`migrate_peer` seam); deliver rest under H | assembly/applied exactly once | **red** (H3.4) |
| `offer_with_a_reused_serial_from_a_new_sync_epoch_is_not_answered_with_old_acks` | incoming incomplete serial 1 origin O1; new Offer serial 1 origin O2 different hash | old transfer dropped, new accepted, no stale ACK | **red** (H3.3) |
| `same_content_after_a_completed_but_unapplied_transfer_is_reoffered` | sender "succeeds" (all chunks ACKed), then a new local change with the same hash | re-offer happens (retry), not swallowed by `last_sent` | **red** (H3.1) |
| `duplicate_offer_retransmission_still_resends_acks` | offer twice, same serial | ACKs repeated, transfer not reset | green now (`sync.rs:174-181`) — keep |
| `one_shot_offer_and_chunk_loss_is_recovered` | drop the first offer and first chunk bursts | applies after retransmission | green now — keep |
| `copy_while_disabled_then_enabled_offers_current_clipboard` | copy with peers removed/disabled, reset, re-add peer + resync | offer | green now (`reset`/`on_applied` clear `last_sent`) — keep |

Extract a pure helper and test it in `input-clipboard/src/lib.rs` (L0, runnable on Linux CI):
`EchoGate`/`OwnSequenceFilter` encapsulating the Windows own-write decision so it can have Linux tests:
`own_write_sequence_is_ignored_once`, `a_user_copy_racing_own_write_is_not_suppressed`,
`notification_read_error_keeps_the_change_pending`.

### 7.2 Transfer/pacing (L0) and protocol (L0)

- `transfer_aborts_after_the_attempt_budget_but_reports_a_retryable_state` — assert failure exposes
  `serial`/snapshot for a retry (new API), not just a bool.
- `commit_ack_is_idempotent_and_serial_matched` (new frame, if task-2 adds it) — duplicate `Applied` frames
  do not double-apply; mismatched serial/hash ignored.
- `offer_is_retransmitted_before_the_first_ack` — keep/extend existing pacing test
  (`transfer.rs:704-729`) for the multi-format case (text sweep finishes before PNG).
- `wire_compat_legacy_peer_never_receives_a_new_frame_type` — capability-gated send: with peer caps
  `CLIPBOARD` only (no new bit) the sender must emit only 12–15 (`lan-mouse-proto/src/clipboard.rs` codec +
  `src/clipboard.rs` sink test).
- `oversized_representation_rejects_only_that_representation` — if H5 fix keeps text when image is too big.

### 7.3 Component/service glue (L2/L3) — needs two small seams

Seam needed (owned by task-2 implementation): a `#[cfg(test)]` way to construct `ClipboardTask`/`run` with
(a) an injectable backend factory (`Box<dyn Clipboard>` + a `FakeClipboard` in `lan-mouse-test-support` or
`src/clipboard.rs` tests), (b) injectable channels/routes, (c) a "step once" driver instead of the 15 ms
`tokio::time::interval` (or `tokio::time::pause`). The verifier harness
`tests/qa_clipboard_atomicity.rs` already mirrors the glue with its `Node`; these service-level tests cover
what it cannot (the real `src/clipboard.rs` state):

| Test name | Scenario | Assertion | Today |
|---|---|---|---|
| `copy_during_backend_startup_is_offered_when_the_backend_becomes_ready` | backend factory pending > transfer abort horizon, local copy, then backend ready | snapshot reaches peer without a second copy (retry loop) | **red** (H2.1/H1) |
| `backend_restart_reoffers_the_current_clipboard_and_re_registers_incoming_peers` | incoming-only peer, copy, force `RunExit::BackendStopped`, recreate | peer re-registered + current snapshot offered; no copy lost | **red** (H2.4) |
| `incoming_route_peer_gets_the_current_clipboard_on_creation` | frames from a listener peer create the synthetic peer; local copy already happened in the same window | resync offer to that peer | **red** (H2.2) |
| `backend_changed_error_schedules_a_bounded_reread` | fake backend returns `Err` for one change, then would return content | content is delivered after the retry, not dropped | **red** (H2.3/H7) |
| `ack_and_cancel_frames_are_not_gated_by_clipboard_read` | peer policy grants `clipboard-write` only; inbound offer arrives | ACKs/`Cancel` sent; transfer completes | **red** (H4) |
| `policy_grants_are_evaluated_per_frame_direction` | read-only / write-only / all; assert content vs control frame behavior | matrix passes | **red** (H4) |
| `disable_mid_transfer_clears_pending_and_re_enable_resyncs` | copy, disable during transfer, enable | peer gets current clipboard once, no stale frames applied | partial today |
| `supervised_retry_after_transfer_abort_uses_backoff` | abort, no user input, peer returns | exactly one re-offer after backoff, no busy loop | **red** (H1) |

L4 serial smoke (extend `src/clipboard.rs::tests`, `clipboard_frames_traverse_dtls_in_both_directions`
`:546-634`, `--test-threads=1`): full bidirectional transfer over real DTLS loopback (offer→chunks→ack→
apply both ways), then a mid-transfer transport drop + reconnect and assert the pending copy is delivered
after reconnect (serial/backoff behavior). Keep this to one serial test; avoid flakiness by using the
existing cert/port helpers.

L1 property test (new `tests/component_clipboard_sync.rs` or `sync.rs` with `proptest`, seed printed):
random interleavings of {local copy, peer add/remove/resync, loss bursts, apply failures, backend restart}
with bounded steps; invariant after quiescence for every direction: *receiver clipboard == last local copy
made by the sender that was not superseded*, and `applied` is monotonic. This is the formal statement of the
atomicity guarantee and catches ordering regressions none of the targeted tests enumerate.

### 7.4 Manual matrix (L5) — hardware validation

Two machines, one Windows 10/11, one Ubuntu (GNOME 50/KDE portal, wlroots, and X11 fallback if available);
`LAN_MOUSE_LOG_LEVEL=debug`; watch `ClipboardStatus` in the UI (backend name, `received`, `local` counters)
and the log lines listed in §9. Run for **both directions** and both host/client roles (cross the mouse
either way, and also copy while the pointer is on the local machine).

1. Copy text in Notepad → paste in Ubuntu editor; reverse (gedit → Notepad2). Expect exact text incl.
   newlines (CRLF↔LF normalization).
2. Copy browser text+image (screenshot with `Ctrl+Shift+C` in Firefox/Chrome) → paste image and text; time to
   paste (H5: expect text delayed until image completes today).
3. Rapid repeated copies: script 50 distinct texts with 100 ms interval on each side (e.g. `for i in $(seq 50);
   do printf "copy-%d" "$i" | xclip -selection clipboard; sleep 0.1; done` on X11 / PowerShell
   `Set-Clipboard` on Windows); assert the last value arrives and `applied` increments; repeat 10×.
4. Copy immediately after clicking Connect / entering the peer screen (H1/H2 window): 10 trials, no recopy
   allowed; expect 10/10 delivered.
5. Restart the peer backend during a copy: `systemctl --user restart xdg-desktop-portal` (portal session
   restart, expect SVC-01 recovery) / kill the Windows listener path by restarting the app; copy within 1 s
   and after; assert recovery without restarting lan-mouse and without a second copy.
6. Wi-Fi roam / link drop: disable/enable the NIC for 1-3 s during a copy; assert the copy is delivered after
   reconnect (today: recopy needed).
7. One-way permissions: grant peer `clipboard-write` only, and the reverse `clipboard-read` only; verify the
   allowed direction works and the other is blocked with a visible status error (H4).
8. Clipboard contention on Windows: run Ditto/Windows clipboard history + Excel copy loop while copying
   (H7/H8); assert no silent loss.
9. GNOME Wayland with portal unavailable (< 50): copy from a native Wayland app; `doctor` must show which
   backend is in use and the limitation must be documented (H10/matrix row).
10. Lock/unlock and session suspend/resume on both machines; copy after resume (portal/EIS session churn,
    H2.1).

Diagnostic signature to record for each trial: source log `clipboard: local change detected` → `clipboard:
syncing ...` → (peer) `clipboard: accepting snapshot` → `clipboard: applied ...`; any missing arrow identifies
the hypothesis (see §9).

---

## 8. Recommended fix architecture (input for task-2)

Capability/wire compatibility constraints are in §5 (additive frames are safe; gate emission on a new
capability bit, e.g. `CLIPBOARD_COMMIT = 1 << 10`, and keep frames 12–15 untouched for legacy peers).

1. **Delivery state machine (`input-clipboard/src/sync.rs`).** Replace global `last_sent`/`last_applied`
   with per-peer state: `pending_snapshot` (the last local snapshot, always kept until *committed* by every
   peer), per-peer `last_delivered_hash` and `last_committed_hash`, and a per-snapshot `generation/serial`
   that is monotonic and **includes a per-sync-instance epoch** (or key incoming transfers by
   `(peer, origin, serial)`) to kill H3.3. `add_peer`/`resync_peer(peer)` must schedule an offer of
   `pending_snapshot` unconditionally. `migrate_peer(from, to)` must move `incoming`/`outgoing` (H3.4).
2. **Apply confirmation (protocol).** Add a capability-gated `Applied { serial, hash }` (or `Commit`) frame:
   receiver sends it *only after* `Clipboard::write` succeeded (`src/clipboard.rs:389-400` →
   `sync.on_applied` path). Sender removes the peer from the pending set on match; on timeout/no-commit it
   retries with exponential backoff (bounded, e.g. 250 ms → 2 s, reset on new copy) instead of aborting
   permanently. Legacy peers: fall back to today's chunk-ACK behavior (document the weaker guarantee).
   Receiver: keep the completed snapshot (and retry `write`) until committed or superseded; on apply failure
   send a negative ack so the sender retries sooner.
3. **Retry/backoff loop (`src/clipboard.rs`).** On transfer failure or missing commit, keep the copy in a
   `pending` slot and re-offer on a timer (the 15 ms ticker can drive it) while connected; do not clear the
   snapshot on abort. Only a newer local copy supersedes it.
4. **Startup/reconnect seeding.** On backend creation/restart and on every peer registration (outgoing *and*
   incoming), offer the current `pending_snapshot` (read once from the backend after creation; keep the read
   result even if peers are absent — the "no peers → drop without recording" branch must store it instead,
   `sync.rs:121-126`). On `RunExit::BackendStopped`/reconnect, re-register incoming peers too (or keep routes
   across restarts where possible).
5. **Backend robustness (platform).** Windows: pure `EchoGate` (sequence captured before write; bounded
   own-write range), retry `changed()` read errors (bounded backoff) instead of consuming the notification,
   avoid dequeuing a notification and then awaiting a cancellable read (H7/H8). Wayland: don't set
   `last_written` before `copy_multi` succeeds (WAY-02 ordering) and clear it after the watcher echo is
   confirmed (H2/WAY-02). Portal: handle the owner-is-us event by deferring/validating the read instead of
   trusting it (H9); bound the owner-change read so transfer serving is not starved. X11: retry a timed-out
   read once (H10). Keep every read/write bounded.
6. **Policy gate (H4).** Classify frames: content (Offer/Chunk) gated by peer read capability; control
   (Ack/Cancel/Applied) gated by the peer write capability (or not gated for frames belonging to a transfer
   we are receiving); unit-test the matrix.
7. **Snapshot granularity (H5, optional but valuable).** Apply text/HTML as soon as their representations
   complete; stream PNG afterwards. Make the sender's abort an idle timeout proportional to the payload
   (or to progress), not a fixed 10 windows.
8. **Observability.** Surface a per-copy state in `ClipboardStatus` (e.g. `pending: bool`, `last_retry`) and
   log copy `generation`/`serial` + peer id on every state transition so L5 trials can be diagnosed from
   logs alone.

---

## 9. Diagnostic checklist for the field (Windows 10/11 ↔ Ubuntu)

1. **Backend is observing the clipboard?** UI `ClipboardStatus` counters: `local` (`detected`) must increment
   on every `Ctrl+C` on the machine; if not → H2/H7/H8/H9/H10 (backend/watcher), check the log for the
   backend name (`using clipboard backend: ...`, `src/clipboard.rs:208`).
2. Key log lines (debug level, `LAN_MOUSE_LOG_LEVEL=debug`):
   - source: `clipboard: local change detected (...)`, `clipboard: syncing ... to N peer(s)`
     (`src/clipboard.rs:414-416`, `sync.rs:150-155`), `clipboard: ignoring a duplicate change event`
     (`sync.rs:118`), `clipboard: local change with no peers, not offering it` (`sync.rs:124`),
     `clipboard snapshot exceeds the size limit` (`sync.rs:109`), `clipboard: peer may not read our
     clipboard, dropping frame` (`src/clipboard.rs:77`).
   - transfer: `clipboard: transfer N to peer P failed` (`sync.rs:286`), `clipboard: ack for serial N ...
     without a transfer` (`sync.rs:234`), `clipboard: merged duplicate peer ...` (`src/clipboard.rs:359`).
   - destination: `clipboard: accepting snapshot N ...` (`sync.rs:191`), `clipboard: rejected completed
     transfer` (`sync.rs:312`), `clipboard: could not apply remote snapshot` (`src/clipboard.rs:391`),
     `clipboard: applied ... from a peer` (`sync.rs:329`), `clipboard: incoming transfer from peer P timed
     out` (`sync.rs:296`).
   - lifecycle: `clipboard backend stopped, trying to restart it` (`src/clipboard.rs:235`), `clipboard
     backend unavailable` (`src/clipboard.rs:151`), `clipboard: local change stream ended`
     (`src/clipboard.rs:423`).
   - Windows: `the Windows clipboard listener did not start` (`windows.rs:316-322`), `could not open the
     Windows clipboard (another app is holding it)` (`windows.rs:113-115`), `windows clipboard is marked as
     secret`.
   - X11/Wayland/portal: `X11: clipboard read timed out` (`x11.rs:377`), `Wayland clipboard watcher stopped`
     (`wayland.rs:132`), `portal clipboard read of ... timed out/failed` (`portal.rs:369-370`).
3. **`doctor` output**: `clipboard portal: vN (consent saved)` and the runtime backend name tell whether the
   Ubuntu side uses portal/data-control/X11 (matrix row); GNOME < 50 Wayland needs a documented caveat
   (`src/doctor.rs:417-444`).
4. Reproduce the timing class deterministically in logs: run the 50-copy loop (§7.4 step 3) with debug
   logging on both sides; count `local change detected` vs `applied` on the destination. A mismatch is a real
   hole in the atomicity guarantee; capture the full log pair as evidence for the final fix.

---

## 10. Open questions / validation needed

1. **Windows own_sequence race window (H7)** — needs Windows hardware + a clipboard manager to quantify;
   confirm whether `WM_CLIPBOARDUPDATE` is delivered per `SetClipboardData` or once per `EmptyClipboard`
   sequence (affects whether the echo filter sees intermediate states).
2. **Portal owner-is-us behavior (H9)** — needs GNOME 50/KDE Wayland session; reproduce by copy-paste across
   the link and then copying in a native app immediately after a remote apply; confirm whether a stale read
   is the failure and whether a deferred re-read fixes it.
3. **GNOME < 50 Wayland fallback** — does the XWayland X11 backend see native Wayland copies in a
   lan-mouse-service context (DISPLAY available)? Affects the supported-matrix statement; `doctor` should
   report the actual backend.
4. **X11 slow/stuck owners (H10)** — which apps exceed the 10 s selection timeout; whether a single retry
   would recover them.
5. **Transfer-abort horizon** — is 10 windows × 120 ms enough on real Wi-Fi/VPN? The previous QA ledger
   already lists this as a residual risk; H1's retry needs a backoff budget measured on hardware (target:
   survive a 5-10 s link drop without a manual recopy).
6. **Serial-epoch collision (H3.3)** — confirm `TRANSFER_TIMEOUT`-lingering incoming transfers actually occur
   on backend restart in the field (log evidence: `ack for serial ... without a transfer` /
   `offer retransmission`).

---

## Appendix A — exact file:line index (verified at `03f2a46`)

Lifecycle / sync:
- `input-clipboard/src/sync.rs:38-44` dedupe rationale; `:49-63` constructor (`last_applied/last_sent/next_serial`); `:65-72` add/remove peer; `:87-93` reset; `:103-127` `on_local_change` (empty/limits/echo/dedupe/no-peers/`last_sent`); `:129-156` serial, drain in-flight, offer; `:158-226` frame dispatch; `:169-171` `last_applied` silent ignore; `:174-181` same-serial offer → resend acks; `:227-249` acks; `:250-257` cancel; `:265-317` tick (failure→`last_sent=None`, timeouts, complete/assemble); `:321-334` `on_applied`.
- `input-clipboard/src/transfer.rs:21-31` constants; `:96-181` poll (offer retransmit, pacing, attempts/abort); `:184-236` on_ack; `:238-244` done/failed; `:269-312` from_offer (limits); `:326-384` chunk/ack; `:386-419` complete/assemble; `:421-424` timeout.
- `input-clipboard/src/lib.rs:97-134` Limits; `:185-197` hash; `:212-229` `Clipboard` trait (write must not wake `changed`); `:358-398` backend selection.
- `lan-mouse-proto/src/clipboard.rs:16-70` frame types + `Ack` semantics; `:86-238` codec; `:106-167` sizes.

Service glue:
- `src/clipboard.rs:31-38` routes; `:48-83` permission gate + sink; `:100-126` channel drain while disabled; `:133-162` backend creation/retries; `:172-241` task spawn/loop/RunExit; `:251-253` drain helper; `:264-309` run init/transport task/15 ms ticker; `:311-330` enable toggle + peer removal/reset; `:336-387` registration (merge, read+offer only for outgoing); `:389-406` apply + status; `:408-428` local change branch; `:429-454` listener frames (synthetic peers); `:455-474` outgoing frames + peer events; `:546-634` DTLS loopback test.
- `src/emulation.rs:642-647` listener→clipboard forward.
- `src/listen.rs:652-657` raw reply; `:861-912` read loop + clipboard policy gate (`:890-899`); `:964-979` incoming policy.
- `src/connect.rs:88-108` ctx fields (`clipboard_tx`, `hello_sent`); `:303-371` clipboard rx/sender handoff; `:487-492` `ensure_connected`; `:556-610` `send_raw`/`send_bytes`; `:1277-1293` Hello; `:1294-1417` receive loop (+ unknown-datagram skip); `:1325-1350` Pong→capabilities/Hello; `:1123-1210` ping round (2 s).
- `src/service.rs:220-260` clipboard wiring; `:935-941` peer disconnect events; `:1312-1355` status handling/toggle.
- `src/client.rs:466-473` active clients; `:638-648` `peer_supports`/`active_addr`; `:114-128` `get_client`.
- `src/config.rs:98-105,1234-1259` clipboard config/limits.

Platform backends:
- Windows: `input-clipboard/src/windows.rs:54-60,92-102` own_sequence; `:104-116` open retries; `:145-211` read; `:213-249` write (sequence at `:247`); `:251-264` changed (echo filter); `:277-323` listener spawn; `:386-401` `wnd_proc`.
- Portal: `input-clipboard/src/portal.rs:33-40` timeouts/budgets; `:154-232` watcher task (owner change `:185-209`, owner-is-us `:192-195`, secret `:196-200`, transfer serving `:212-229`); `:258-265` read (last_mimes); `:267-287` write; `:289-294` changed; `:331-375` read_selection.
- Wayland: `input-clipboard/src/wayland.rs:88-137` watcher thread (secret `:110-116`, `last_written` hash `:123-125`, cleared `:129`); `:139-177` read_offer; `:185-187` read; `:189-220` write (`last_written` set at `:191`, copy at `:210-216`, publish at `:218`); `:222-224` changed.
- X11: `input-clipboard/src/x11.rs:54-64` timeouts/sizes; `:153-205` thread/atoms/XFixes setup; `:214-224` initial read; `:226-353` event loop; `:332-336` SelectionClear; `:337-350` XFixes owner change; `:355-373` polling fallback; `:375-386` deadlines; `:393-403` `complete_read`; `:428-451` `start_read`; `:459-599` Notify/INCR read; `:685-803` reply/INCR write; `:867-887` `Clipboard` impl.

## Appendix B — proposed regression-catalog entries (do not edit `docs/testing/regressions.md` here; lead/impl to add)

| ID | Symptom | Root cause | Test | Status |
|---|---|---|---|---|
| R-27 | First `Ctrl+C` after connection/peer app start sometimes never reaches the peer; second copy fixes it | no apply/commit ack + 1.2 s abort with no automatic retry | `copy_with_no_ack_is_retried_until_the_peer_applies`, `copy_during_backend_startup_is_offered_when_the_backend_becomes_ready` | todo |
| R-28 | New/reconnected peer keeps stale clipboard until a different content is copied | global `last_sent` never reconciled per peer; registration push only for outgoing clients | `newly_registered_peer_is_offered_the_current_snapshot`, `reconnect_after_interrupted_transfer_resends_the_same_content` | todo |
| R-29 | Incoming transfer stalls mid-flight around connect/handoff | duplicate-peer merge drops `incoming`/routes without migration | `incoming_transfer_survives_peer_identity_migration` | todo |
| R-30 | Write-only clipboard permission direction never works | ACK/Cancel gated by read capability | `ack_and_cancel_frames_are_not_gated_by_clipboard_read` | todo |
| R-31 | Windows source occasionally misses a copy or replays stale content | own_sequence race / consumed notification on read error | `EchoGate` unit tests + L5 matrix | todo |

---

## Implementation notes (task-2, applied on top of `03f2a46`)

Scope: `input-clipboard/src/{lib,sync,transfer,portal,windows}.rs`, `lan-mouse-proto/src/clipboard.rs`,
`src/{clipboard,listen,service}.rs`. No frame layout/version change; old clipboard-capable peers keep working
(the only peer-visible change is a repeated `Offer`, which every build that speaks the clipboard extension
already answers with a re-ack).

| Finding | Fix |
|---|---|
| H1 / P0 — `Ack` means "chunks arrived", not "applied"; a failed receiver write leaves the sender believing success | Receiver withholds the final `Ack` (per transfer) and releases it from `ClipboardSync::on_applied` only after the backend write succeeded; `on_apply_failed` sends `Cancel` instead. Service retries a failed `backend.write()` with bounded backoff (`PendingApply`, ~4.7 s) before cancelling. |
| H1 / P0 — transfer aborts after ~1.2 s and the sync layer never retries | Retransmission backoff 120/240/480 ms with a ~1.7 s per-transfer budget (`retransmission_delay`), and a sync-level pending re-offer every 2.5 s (max 20) so a silent link outage recovers without a second Ctrl+C. |
| H2 / P0 — incoming-route peers never get the current snapshot on registration | `ClipboardSync::offer_current_to(peer, snapshot)` bypasses the duplicate-change dedupe; the service calls it for every newly registered peer (outgoing registration, incoming `Hello`, lazy incoming frame, reconnect, backend restart) with a throttled read retry. Incoming peer capabilities are shared from `Service::incoming_peer_caps` (now `Arc<RwLock<HashMap<SocketAddr, u32>>>`) with the clipboard task. |
| H3 / P1 — global `last_sent` swallows reconnect/resync and same-content recopy | Replaced by a per-peer `offered: HashMap<PeerId, [u8;32]>`; duplicate-change dedupe now only skips peers that already received the hash. `remove_peer` also drops that peer's incoming transfer/deferred commit. |
| H3 / P1 — `last_applied` never invalidated by a genuinely new local copy | `on_local_change` clears `last_applied` when a different hash arrives; re-copying previously applied content is offered again. |
| H3.3 / P1 — serial epoch collision fake-acks a new instance's first offer | `ClipboardSync` starts at a per-origin serial epoch (`serial_epoch`); a repeated `Offer` only re-acks when serial **and** origin **and** hash match, otherwise it replaces the transfer. |
| Duplicate addr/outgoing peer collapse tears down in-flight transfers | `ClipboardSync::migrate_peer` moves pending outgoing/incoming/offered/deferred state; the service merges address peers by IP (`take_address_peers_for_ip`) when the outgoing `Hello` completes. |
| L5 / P0 — Windows read error consumes the change notification | `WindowsClipboard::changed` retries the read (3 × 40 ms on top of the ~200 ms open retry); the service also retries with `input_clipboard::read_retrying` when `changed()` reports an error. |
| Portal owner-change event swallowed when data is not ready | `read_selection_retrying` re-reads up to two times (fast failures only) after an owner-change event. |
| R-30 — Ack/Cancel wrongly gated by clipboard permissions | `ClipboardFrame::is_content()`; the listener gate and `Sink::send` only apply the read/write permission to `Offer`/`Chunk`. |

Residual risks kept as follow-up: X11 read failures/timeouts still rely on the next owner change; a receiver that
loses the sync task between receiving and applying recovers through the sender's re-offer (≤ ~2.5 s + backoff), not
via a dedicated commit protocol; a simultaneous reconnect/backend-restart where both sides re-offer different
content still resolves by last writer (no cross-peer timestamp on the wire).

### Verifier follow-up (task-5, same dirty tree at `03f2a46`)

Three divergence edges found by the independent verifier were fixed on top of the above:

1. **Stale auto-retry after a remote apply.** A remote snapshot we applied is the last-writer event: `on_applied`
   now cancels in-flight outgoing transfers carrying a different hash and drops a pending automatic re-offer with a
   different hash (`drop_superseded_retry`, also invoked from `on_apply_failed`). Otherwise a copy scheduled for
   re-offer during an outage could later overwrite the peer's newer clipboard (A=Y, B=X divergence).
2. **Late frames after a disconnect.** `ClipboardSync::handle_frame` only accepts frames from currently registered
   peers, so a complete transfer queued before a disconnect cannot recreate an incoming transfer and apply stale
   content. Write-only/incoming-route devices are now *registered* peers with a separate `may_receive` flag
   (`set_peer_may_receive`): their inbound content and ACKs pass the gate, but our clipboard is never offered to them.
3. **Pending clipboard write vs. a newer local copy.** The service cancels a `PendingApply` when a local change is
   detected (`cancel_pending_apply` -> `ClipboardSync::on_apply_failed`), so a failed remote write retried later can
   no longer overwrite the user's newer copy.
