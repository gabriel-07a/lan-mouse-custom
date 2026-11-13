# Bidirectional handoff churn — root cause analysis

Task: `task-9` (read-only investigation; no production edits, no push/CI/release).
Author: bidirectional-analyst. Date: 2026-11-13 (America/Sao_Paulo, UTC−03:00).

Symptom under investigation:

- Ubuntu source → Windows receiver: `entering client 0` → `first_motion` →
  `LeaveAt` / `leaving client 0` after ~2 s (and, in bursts, within the same
  second), repeating.
- Windows source → Ubuntu receiver at 16:15:33: repeated
  `handoff: return request ... along=None` while Windows controls.
- Question: is the churn explained by the old Windows 0.2.18 receiver, by a
  late reverse residual that escapes the 200 ms source guard, or by repeated
  capture re-entry? Would a same-build "0.2.19" (current `main` on both sides)
  remove it, or is a position-aware source/receiver guard still required?

**Binary answer up front: a position-aware guard is still required. A
same-build 0.2.19 alone is not expected to remove the churn.** The detailed
argument, with the exact code/build facts it rests on, is in §6 and §7.

---

## 1. Build / environment matrix

| Endpoint | Build | Commit | Notes |
|---|---|---|---|
| Ubuntu (`predator`, this machine) | `lan-mouse 0.2.18`, branch `main`, `commit_hash:a3e1ad81`, built 15:57:05 | `a3e1ad8` | Installed at `~/.local/bin/lan-mouse-custom` at 15:58. Includes the final 200 ms reverse-only source guard (`a3e1ad8`), the Linux receiver fixes (`8aebfdf`) and the shutdown/cursor-hide work. |
| Windows (`DESKTOP-GABRIEL`, 192.168.166.166) | release 0.2.18 | `5233b05` | Old source **and** old receiver: no source-side reverse guard; Windows receiver code is identical to current `main` (see §5.1). |

Config (Ubuntu): `~/.config/lan-mouse/config.toml` — one client
`DESKTOP-GABRIEL` / `192.168.166.166` at `position = "right"`; device
`capabilities = 31`, negotiated `caps=Some(1016)` at runtime. Both sides are
same-LAN IPv4, DTLS/UDP 4242.

Process/build history from `journalctl --user -b` (relevant window):

| PID | alive | binary in use | trace enabled |
|---|---|---|---|
| `107865` | 14:24:39 → 15:58:25 | 14:24 build (budget-based reverse guard; pre-final `a3e1ad8`) | yes (`LAN_MOUSE_HANDOFF_TRACE` set for the systemd unit) |
| `193051` | 15:58:26 → 16:15:15 | `a3e1ad8` (final 200 ms window-only guard) | yes (trace rows until 16:12:52) |
| `195582` | 16:15:21 → … | `a3e1ad8` | **no** — launched from the `.desktop` entry, so it did not inherit the systemd user environment |

Consequences for the evidence: the terminal `handoff-trace.jsonl` stops at
`16:12:52.869`; the 16:15:26–16:15:53 episodes (the interesting ones) are
**journal-only**. `~/.cache/lan-mouse/handoff-trace.jsonl`:
4678 lines, 11:52:05 → 16:12:52; `handoff-trace.jsonl.1`: 35190 lines,
2026-11-11 14:28 → 2026-11-13 11:50.

---

## 2. Evidence and method

Commands used (read-only):

```sh
journalctl --user -b --no-pager -o short-iso | grep -v dsh-subprocess
~/.cache/lan-mouse/handoff-trace.jsonl{,.1}          # parsed as JSON Lines
git diff --stat v0.2.18..main -- <handoff paths>     # same-build delta
git show v0.2.18:<path>                              # old receiver/source code
grep -n 'handoff_trace::event' src input-emulation input-capture
```

Limitations (kept explicit so the recommendation is not over-read):

1. No Windows-side log or trace was available from this machine; every
   `LeaveAt` / `request_handoff` observation is the Ubuntu peer's view.
2. The 16:15 churn ran without `LAN_MOUSE_HANDOFF_TRACE` (starter process
   `195582` was launched by the desktop file, not by systemd), so there is no
   structured delta-level evidence for the final-guard build in that window.
   The trace-backed delta evidence is from the 14:24 build (`107865`) and from
   earlier `main` builds in `.jsonl.1`.
3. Journal timestamps have 1 s resolution; millisecond timing below comes from
   the trace files (`ts_unix_us` / `elapsed_us`), and the 16:15 intervals are
   bounded to ±1 s.

---

## 3. Direction B — Windows 0.2.18 source → Ubuntu receiver (new `main`)

### 3.1 Exact timeline at 16:15 (the `along=None` storm)

All lines below are `lan-mouse-custom.desktop[195582]` (build `a3e1ad81`),
verbatim journal messages:

```
16:15:26  releasing capture: 192.168.166.166:51973 entered this device
16:15:26  absolute pointer armed at (1535,532.44446) region 0,0..1536,960 gain 1.00x (accel mirrored); push the cursor past the Right edge to hand control back
16:15:28  pushed past the Right edge; handing control back
16:15:28  handing control back: remote pointer left through Right (192.168.166.166:51973)
16:15:28  handoff: return request to 192.168.166.166:51973: supported=true along=Some(0.41443634) caps=Some(1016)
16:15:28  peer at 192.168.166.166:51973 released its capture (return hand-off)   [x3]
16:15:28  releasing capture: 192.168.166.166:51973 entered this device
16:15:28  absolute pointer armed at (1535,400.8889) ...
16:15:29  pushed past the Right edge; handing control back
16:15:29  handoff: return request ... along=Some(0.4430541)
16:15:29  peer at 192.168.166.166:51973 released its capture (return hand-off)   [x3]
16:15:29  pushed past the Right edge; handing control back
16:15:29  handoff: return request ... along=Some(0.47051328)
16:15:29  peer at 192.168.166.166:51973 released its capture (return hand-off)   [x3]
16:15:30  releasing capture: 192.168.166.166:51973 entered this device
16:15:30  absolute pointer armed at (1535,357.3333) ...
16:15:33  handoff: return request to 192.168.166.166:51973: supported=true along=None caps=Some(1016)   [x10]
16:15:33  peer at 192.168.166.166:51973 released its capture (return hand-off)   [x10]
16:15:35  entering client 0 ...
16:15:35  client 0 acknowledged the connection!
```

Same pattern recurs over the whole boot: 251 × `pushed past the Right edge`
⟷ 251 × `handoff: return request ... along=Some`, plus 33 ×
`along=None`; 829 × `peer ... released its capture (return hand-off)`.
`along=None` bursts (per second): 2 each at 10:58:32, 11:03:28, 11:07:48,
11:37:25, 11:42:09, 11:47:10, 11:57:21, 13:51:11; **7 at 11:48:46** and
**10 at 16:15:33**. The 1 Hz version is visible at 15:40:52–15:41:12 (10
consecutive cycles): `absolute pointer armed at (1535,…)` → `pushed past the
Right edge` → `handoff: return request ... along=Some(...)` → `releasing
capture: … entered this device` → repeat.

### 3.2 What each kind of request means (code paths)

- `along=Some(...)` — `input_emulation::libei` absolute tracker detected the
  *injected* pointer being pushed outward past the entry edge
  (`input-emulation/src/libei.rs:933` `pushed past the {edge:?} edge; handing
  control back`), forwarded as `EmulationEvent::EdgeReturn`
  (`src/emulation.rs:1327`) to `request_return` → `request_handoff(addr,
  Along)` (`src/service.rs:1041,1434,1461`). Log site
  `src/service.rs:1472`. On the wire it becomes `LeaveAt { serial, along }`.
- `along=None` — **not** an edge return. It is the single other call site,
  `request_handoff(addr, None)` in
  `src/service.rs:1301`, reached only from
  `ICaptureEvent::CaptureBegin(handle)` for an **incoming** connection
  (`src/service.rs:1291-1303`): "we entered the capture zone for an incoming
  connection => notify it that its capture should be released". On the wire it
  becomes `Leave { serial }`.

The suppression that exists for this is deliberately narrow:

```rust
// src/capture.rs:1604-1614
/// Only the incoming [`CaptureType::EnterOnly`] capture may react to a begin in
/// that state: it sends the `Leave` that ends the hand-off.
fn is_suppressed_remote_begin(
    remote_peer: Option<Position>,
    capture_type: CaptureType,
    event: &CaptureEvent,
) -> bool {
    remote_peer.is_some() && capture_type == CaptureType::Default && event.is_begin()
}
```

i.e. while a peer drives this machine (`remote_peer.is_some()`), a `Begin` on
an outgoing capture is dropped, but a `Begin` on the **EnterOnly** incoming
capture is intentionally allowed and immediately turns into
`request_handoff(addr, None)`. The incoming zone is at the same shared edge,
and the injected remote pointer has just been parked at that edge
(`absolute pointer armed at (1535,…)` — 1 px inside the 1536-px screen). So
any residual/jitter that re-arms the barrier is read as "the local user wants
control back": the receiver asks the (already-driving) peer to release, the
peer releases (`peer ... released its capture`), the release clears
`remote_peer`/re-enables local capture, the parked cursor is still on the
edge, and the next `Begin` fires again. That is the 7–10 events/s
`along=None` loop. It is a **receiver-side re-entry loop**, not a position
report and not the source's `LeaveAt`.

`peer ... released its capture` appears 2–3× per request because
`request_handoff` deliberately sends the first request twice
(`src/service.rs:1482-1483`, "send the first request twice") and retries are
idempotent by serial; that duplication is benign noise, not a separate cause.

### 3.3 Why the old Windows *source* makes it worse

`v0.2.18` (`5233b05`) has **no reverse-motion guard at all** in
`src/capture.rs` (grep for `HANDOFF`, `residual`, `reverse_motion`,
`backswing` in `git show v0.2.18:src/capture.rs` → no match). Every
compositor warp / touchpad backswing delta the Windows source produces at the
edge is forwarded. The Ubuntu receiver on current `main` is deliberately very
sensitive to exactly that: `8aebfdf` changed the libei absolute tracker from
`RETURN_MARGIN_PX = 3.0` / `EDGE_ANCHOR_INSET = 24.0` to `0.0` / `2.0`
(`input-emulation/src/libei.rs:148,255`) so that a single outward pixel
after the 1–2 px-inside anchor hands control back (the commit's stated goal:
"no extra force"). An unguarded old source + the new one-pixel-sensitive
receiver is therefore a return-request generator; the 16:15:28–16:15:30
`along=Some` burst is exactly this.

Same-build relevance: a same-build upgrade **does** fix this half — it puts
`a3e1ad8`'s source guard on the Windows source. It does **not** change the
receiver-re-entry loop of §3.2.

---

## 4. Direction A — Ubuntu `a3e1ad8` source → Windows 0.2.18 receiver

### 4.1 Exact timeline at 16:15 (final-guard build, no trace)

All lines `p195582` (`a3e1ad81`):

```
16:15:35  entering client 0 ...
16:15:35  client 0 acknowledged the connection!
16:15:37  releasing capture at return coordinate 0.585 (LeaveAt(83))
16:15:37  leaving client 0 ...
16:15:37  sending Leave event to client 0
16:15:39  entering client 0 ...
16:15:39  client 0 acknowledged the connection!
16:15:41  releasing capture at return coordinate 0.508 (LeaveAt(84))
16:15:41  leaving client 0 ...
16:15:41  sending Leave event to client 0
16:15:41  entering client 0 ...
16:15:41  client 0 acknowledged the connection!
16:15:43  releasing capture at return coordinate 0.475 (LeaveAt(85))
16:15:43  leaving client 0 ...
16:15:45  ... LeaveAt(86)
16:15:48  ... LeaveAt(87)         (3 s gap, then same-second enter/leave cycles:)
16:15:48  entering client 0 ... / LeaveAt(87) / leaving client 0
16:15:49  entering client 0 ... / LeaveAt(88) / leaving client 0
16:15:50  entering client 0 ... / LeaveAt(89) / leaving client 0
16:15:51  entering client 0 ... / LeaveAt(90) / leaving client 0
16:15:52  entering client 0 ... / LeaveAt(91) / leaving client 0
16:15:53  entering client 0 ... / LeaveAt(92) / leaving client 0
16:15:55  releasing capture: 192.168.166.166:51973 entered this device   (Windows re-enters)
```

Two things are important:

1. Every `Enter` is acknowledged immediately (`client 0 acknowledged the
   connection!`), so the 2 s `ENTER_TIMEOUT` (`src/capture.rs:362`) is **not**
   the release trigger. The release is a real `ProtoEvent::LeaveAt` received
   from the Windows receiver (`src/capture.rs:837-842`).
2. `LeaveAt(serial)` in the wire format is the **receiver's own return-request
   serial** (`service.next_return_serial()` → `return_serial`), not an echo of
   the Ubuntu `Enter` serial. That is why the trace can show
   `capture.enter_sent serial=1..9` while the journal shows
   `LeaveAt(75..78)` for the same episodes — do not try to pair those counters
   across sides.
3. The first cycles are ~2 s apart and later ones collapse to same-second
   (≈1–2 Hz) `Enter → LeaveAt` pairs; a human repeating a gesture does not
   produce a 2 Hz machine-like cadence, so this is a ping-pong, not intentional
   hand-backs.

### 4.2 What the trace shows for the churn episodes (ms-level evidence)

Parsed from both trace files (95 `capture.first_motion_sent` samples total):

- Latency `Begin → first forwarded motion` is bimodal: 65/95 samples are
  ≤ 0.3 ms (the warp/backswing itself is forwarded as the "first motion"),
  while **30/95 are 204.0–205.4 ms** (plus a handful at 225–785 ms). The
  204–205 ms cluster sits exactly at the end of the 200 ms
  `HANDOFF_REVERSE_GUARD`: everything before it was suppressed/absent, and the
  first forwarded motion lands *on or after* the window boundary.
- `capture.reverse_motion_dropped` fired only 6 times in ~4.5 h of trace:
  `-17.96 px` (15:03:23.310), `-10.59 px` (15:19:10.270), `-15.22 px`
  (10:57:35.075), `-16.94 px` (11:03:29.451), `-1.41 px` (11:37:30.358),
  `-1.07 px` (11:37:31.116). Big backswings are real, and the guard only sees
  a subset of them.
- The 15:03 episode (14:24 build, budget guard, not the final one) is the
  clearest leak record — after a `-17.96 px` drop, the very next forwarded
  motion is `+12.39 px`; the next `Enter` episode (serial 3) forwards
  `-63.27 px` as its *first motion* (a reverse warp larger than the 32 px
  budget treated as a "deliberate hand-back"), followed by `+65.34 px`. This is
  exactly the `-63.27 px` / `34.9 px` class of activation warps that the final
  `a3e1ad8` guard is meant to stop.
- Churn episodes correlate with the 0.1 ms class too: 10:33, 10:51, 10:53,
  10:57–10:58, 11:03 show alternating ±60–82 px "first motion" deltas at
  0.1 ms and are the same seconds as `along=None` bursts — i.e. the two
  endpoints are bouncing Enter/Leave faster than any human cadence.

Interpretation: the source-side guard is *window*-based, not *position*-based.
It correctly drops reverse deltas for 200 ms, but the hand-off onset in the
observed sessions is not over at 200 ms (the 204–205 ms cluster shows the
settling boundary is at least that long), and any reverse residual after the
boundary — or a warp the guard never saw — is forwarded whole. The Windows
receiver does not need much: in `main`/`0.2.18` it parks the absolute pointer
1–2 px inside the entry edge (`entry_anchor_fraction_in_monitors(..., 1)`,
`input-emulation/src/windows.rs:341`; `ENTRY_ANCHOR_MARGIN` only for the
legacy `along=None` path) and fires at
`EDGE_RETURN_MIN_OVERSHOOT = 2` physical px
(`input-emulation/src/absolute.rs:149`). A ~3 px outward target is enough
for a `LeaveAt`.

### 4.3 Not the cause (ruled out)

- **`ENTER_TIMEOUT` (2 s) on the source** — every churn `Enter` is acked
  immediately; `LeaveAt` is a real peer packet.
- **Protocol/version mismatch** — `lan-mouse-proto` is unchanged between
  `v0.2.18` and `main`; both sides negotiate `caps=1016`, both support
  `ENTER_ACK`, `SERIALIZED_LEAVE`, `RETURN_COORDINATE`. The Windows peer is
  not a legacy (`Enter`-fire-and-forget) peer.
- **`client 0 acknowledged` vs `Enter` retransmit** — no stale-ack loop
  visible; retransmissions stop on the ack.

---

## 5. Code facts that drive the conclusion

### 5.1 Windows receiver is identical in `v0.2.18` and `main` (`a3e1ad8`)

```text
$ git diff --stat v0.2.18..main -- \
    input-emulation/src/windows.rs input-emulation/src/absolute.rs \
    input-capture/src/windows input-capture/src/windows/* lan-mouse-proto/src/lib.rs
(no output — identical)
```

Full same-build delta `v0.2.18..main` (all files):

```text
docs/testing/*.md, scripts/qa-handoff-old-receiver.sh, scripts/qa-handoff-shutdown.sh
input-capture/src/libei.rs          # Linux capture
input-clipboard/src/lib.rs, src/clipboard.rs
input-emulation/src/libei.rs        # Linux receiver sensitivity (3.0/24.0 -> 0.0/2.0)
lan-mouse-gpui/src/cursor_extension.rs, linux/gnome-extension/**
src/capture.rs                      # source guard (all platforms) + lifecycle
src/cursor_hide.rs, src/service.rs  # cursor hide/park, return-handoff retries
tests/qa_handoff_*.rs
```

There is **no Windows receiver/emulation change**: `ENTRY_ANCHOR_MARGIN`,
`EDGE_RETURN_MIN_OVERSHOOT = 2`, the 1-px `along` anchor and the
`EdgeReturnLatch` throttle/budget are byte-identical to `v0.2.18`. The only
same-build effect on the Ubuntu→Windows direction is on the **source** side
(`src/capture.rs` on Windows) and on service-level retry/cursor-park
bookkeeping (`src/service.rs`). The return trigger itself — the component that
emits the `LeaveAt` seen in §4.1 — is not changed by 0.2.19.

### 5.2 Ubuntu receiver re-entry path is in `main` and is what produced `along=None`

See §3.2. `main` intentionally lets `CaptureType::EnterOnly` begins through
while `remote_peer` is set; the resulting `request_handoff(addr, None)` loop is
in the newest code (the 16:15:33 run is `a3e1ad8`). A same-build upgrade cannot
remove it because both sides already run that code.

### 5.3 Source guard `a3e1ad8` is time-only

`HANDOFF_REVERSE_GUARD = 200 ms` (`src/capture.rs:390`), fixed from `Begin`,
not extended by dropped deltas, reverse-only, forward never delayed, all reverse
deltas inside the window dropped regardless of magnitude; after the window
reverse motion passes untouched. No positional state is consulted (only the
`remaining_px` telemetry). Therefore a reverse residual that arrives at
> 200 ms — or an activation warp that arrives in bursts around the window
boundary, as in the 204–205 ms cluster — is structurally uncovered.

---

## 6. Ranked causes

| Rank | Cause | Evidence | Fixed by same-build 0.2.19? |
|---|---|---|---|
| 1 | **Ubuntu source → Windows receiver churn is a fixed-window guard vs. a receiver that fires from ~3 px.** The source suppresses reverse motion only for 200 ms; activation warps/backswings can outlast that (30/95 trace samples show the first forwarded motion at 204–205 ms) and any late reverse delta reaches a receiver parked 1–2 px inside the edge with a 2-physical-px overshoot threshold. The final guard was already running for the 16:15:35–16:15:53 churn (`LeaveAt(83..92)`). | `src/capture.rs:390,1126-1230`; `input-emulation/src/absolute.rs:149`; `input-emulation/src/windows.rs:341`; trace `2026-11-13T15:03:22.204…15:03:24.138`; journal 16:15:35–16:15:53 | **No.** Windows receiver code identical in `main`; the guard is on the Ubuntu source already. |
| 2 | **Ubuntu receiver-side capture re-entry loop** (Windows→Ubuntu direction): `ICaptureEvent::CaptureBegin` on the EnterOnly incoming capture while `remote_peer.is_some()` → `request_handoff(addr, None)` (`Leave`) → peer releases → local capture re-enabled with the cursor parked on the shared edge → `Begin` again. 33 `along=None` requests over the boot, bursts up to 10/s at 16:15:33 and 7/s at 11:48:46, each followed by 2–3 `peer ... released its capture` lines. | `src/service.rs:1291-1303,1434-1489`; `src/capture.rs:1604`; journal 16:15:33, 11:48:46 | **No** — both sides already run this `main` code (16:15 run = `a3e1ad8`). |
| 3 | **Old Windows 0.2.18 source has no reverse guard** → it feeds the now one-pixel-sensitive Ubuntu receiver (0.0 margin / 2 px inset) unguarded activation residuals, producing the 1 Hz `along=Some` return-request series (e.g. 15:40:52–15:41:12) and part of the 251 edge returns. | `git show v0.2.18:src/capture.rs` (no guard constants); `input-emulation/src/libei.rs:148,255`; journal 15:40:52–15:41:12 | **Yes, this half** — same-build puts `a3e1ad8`'s guard on the Windows source. |
| 4 | **Ping-pong amplification / Enter-ack lifecycle noise**: same-second `Enter → ack → LeaveAt` cycles, duplicate `Leave` confirmations (each request is sent twice), repeated re-entry at ~1–2 Hz. Not a root cause; it is the observable consequence of (1)+(2) on a LAN with sub-ms RTT. | journal 16:15:48–16:15:53; `src/service.rs:1482-1483` | No (consequence; disappears if the triggers stop). |

Ruled out: `ENTER_TIMEOUT` (all Enters acked), protocol incompatibility
(proto unchanged, caps equal), IPv4/IPv6 duplicate sessions (single
`192.168.166.166:51973` session in the 16:15 window).

---

## 7. Binary recommendation: **guard, not same-build alone**

**Recommendation: a same-build 0.2.19 should be done as the first step, but it
is NOT sufficient — a position-aware guard is still required.**

Rationale, in the order the evidence supports it:

1. **Same-build changes nothing on the Windows receiver.**
   `git diff v0.2.18..main` for `input-emulation/src/windows.rs`,
   `input-emulation/src/absolute.rs`, and `input-capture/src/windows/*` is
   empty; the protocol is unchanged; therefore the endpoint that emits the
   `LeaveAt` in §4.1 keeps its 1–2 px anchor and 2-physical-px overshoot. If
   the Ubuntu→Windows churn were caused only by "old receiver", same-build
   would fix it — it cannot, the receiver is the same code in both builds.
2. **The final guard was already present during the worst observed churn.**
   The 16:15:35–16:15:53 `LeaveAt(83..92)` cycles ran on `a3e1ad81`; the guard
   is a fixed 200 ms reverse-only window, and trace samples show the hand-off
   settling reaching the 200 ms boundary (first forwarded motion at
   204.0–205.4 ms in 30/95 samples). A late residual is forwarded and the
   unchanged Windows receiver returns. Repeating the same 200 ms experiment
   will keep the same outcome.
3. **The receiver-side `along=None` loop is independent of the source guard**
   and exists in `main` on both sides; same-build keeps it.

What to implement (task-10 scope):

- **Source side (`src/capture.rs`)** — replace/augment the fixed window with a
  *position-aware leading-residual filter*: while the hand-off is settling,
  drop reverse deltas when the tracked pointer is still inside the entry
  band (e.g. within ≤ 8–12 logical px of the crossing edge / last known entry
  coordinate); forward deltas immediately at all times; once the pointer has
  moved clearly inward (outside the band), reverse motion passes — that is the
  user's deliberate return, no time-only force. Keep the 200 ms window only
  as a hard upper bound / telemetry, not as the sole decision.
- **Receiver side (`input-emulation/src/libei.rs` + the `EnterOnly` path in
  `src/capture.rs` / `src/service.rs`)** — gate spurious `CaptureBegin` on the
  incoming (EnterOnly) capture while `remote_peer.is_some()`: require the
  local pointer to be a few px inside the screen and moving inward, and/or
  debounce begins for ~250–300 ms after a remote `Enter`/`peer released`, so
  the parked injected pointer on the shared edge cannot trigger
  `request_handoff(addr, None)` in a loop. A deliberate grab from the local
  user must still work (test with an inward motion beyond the deadband).
- **No wire/protocol change** is needed; both directions use existing events.
- Same-build 0.2.19 (Windows `5233b05` → current `main`) should still be
  scheduled, because it removes cause #3 (unguarded Windows source) and makes
  the two sides run the same source guard; it is a prerequisite of a clean
  bidirectional test matrix, not the fix.

Manual acceptance matrix for the fix (run on the actual Windows box):

| # | Action | Pass |
|---|---|---|
| T1 | Ubuntu → Windows 20×, normal and fast flick | cursor hides on Ubuntu; exactly one `Enter`, no `LeaveAt` unless the user actually pulls back; trace: no reverse delta forwarded inside the entry band |
| T2 | Windows → Ubuntu 20×, normal | no `along=None` burst; at most one `return request` per intentional hand-back; cursor hides on Windows |
| T3 | immediate return (< 200 ms) both directions | return works, ≤ ~250 ms delay, no extra force |
| T4 | park cursor at the shared edge for 5 s while the peer drives | zero `handoff: return request ... along=None`; zero `LeaveAt` |
| T5 | run from the desktop (not systemd) with `LAN_MOUSE_HANDOFF_TRACE=1` exported in the session environment | trace file has the churn window (fixes the evidence gap in §2) |

---

## 8. Residual evidence gaps / follow-ups

1. Re-run the failing handoff with `LAN_MOUSE_HANDOFF_TRACE=1` set in the
   **graphical session** environment (e.g. `~/.config/environment.d/` or launch
   from a terminal) so GUI-launched instances trace; the current
   `~/.cache/lan-mouse/handoff-trace.jsonl` stops at 16:12:52 while the bug
   window is 16:15:26–16:15:53.
2. Capture Windows-side logs (`%LOCALAPPDATA%\lan-mouse\handoff-trace.jsonl`,
   console log) during T1/T2 to pin the exact 2 s `LeaveAt` trigger; the
   Ubuntu view proves the receiver fired but not which input delta crossed
   `EDGE_RETURN_MIN_OVERSHOOT`.
3. The 2 s cadence of the first 16:15:35 cycles is consistent with the
   user re-testing after each bounce; the 2 Hz same-second cycles at
   16:15:48–16:15:53 are the machine ping-pong signature. Do not tune any
   timeout to 2 s based on this journal window alone.
4. Keep the `32 px` value as telemetry only (as `a3e1ad8` does); do not
   reintroduce it as a decision bound.
