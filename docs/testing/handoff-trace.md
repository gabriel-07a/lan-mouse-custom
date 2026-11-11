# Handoff trace (cursor stutter diagnosis)

`LAN_MOUSE_HANDOFF_TRACE=1` enables a small number of structured JSON Lines
around the mouse hand-off. It is independent of `LAN_MOUSE_LOG_LEVEL`; leave
normal logging at `info` to avoid noise.

The detailed file is written to:

- Linux: `${XDG_CACHE_HOME:-~/.cache}/lan-mouse/handoff-trace.jsonl`
- Windows: `%LOCALAPPDATA%\lan-mouse\handoff-trace.jsonl`

It rotates at 5 MiB (one `.1` backup). `LAN_MOUSE_HANDOFF_TRACE_FILE` overrides
the path. The journal only gets a compact one-line summary per event; use the
file for analysis.

Schema:

```json
{"ts_unix_us":1700000000000000,"role":"receiver","stage":"receiver.armed",
 "serial":109,"peer":"192.168.1.42:52706","edge":"Right","along":"0.37"}
```

Summarize sessions with:

```sh
scripts/analyze-handoff-trace.py
```

## Stages

Controller (the PC whose cursor crosses the edge):

```
capture.begin:            client=... pos=... along=...
capture.enter_queued:     client=... serial=... elapsed_us=...
capture.enter_sent:       client=... serial=... elapsed_us=...
capture.first_motion_sent client=... elapsed_us=... dx=... dy=...
```

Receiver (the PC that gets the cursor):

```
receiver.enter_received:  addr=... serial=... pos=... along=...
receiver.armed:           addr=... serial=... edge=... along=...
receiver.ack_sent:        addr=... serial=...
receiver.first_input:     addr=... elapsed_us=...
receiver.input_gap_exceeded: addr=... gap_us=... threshold_us=8000
```

`elapsed_us` is measured from the local stage marker; `receiver.input_gap`
warns when two injected inputs are more than 8 ms apart during the first
300 ms after arming.

## Linux

```sh
systemctl --user set-environment LAN_MOUSE_HANDOFF_TRACE=1
systemctl --user restart lan-mouse-custom.service
# reproduce the crossing, then:
journalctl --user -u lan-mouse-custom.service --since "5 min ago" | grep handoff_trace
# detailed structured file:
scripts/analyze-handoff-trace.py
```

Disable afterwards:

```sh
systemctl --user unset-environment LAN_MOUSE_HANDOFF_TRACE
systemctl --user restart lan-mouse-custom.service
```

## Windows

Set the user environment variable and restart Lan Mouse from a console so the
daemon's log is captured:

```powershell
[Environment]::SetEnvironmentVariable("LAN_MOUSE_HANDOFF_TRACE", "1", "User")
# then launch the app from a terminal, reproduce, and save the console output
```

## What to send

One crossing from host to client with both logs:

1. controller `capture.begin` → `capture.first_motion_sent`;
2. receiver `receiver.enter_received` → `receiver.armed` → `receiver.ack_sent` → `receiver.first_input`;
3. any `receiver.input_gap_exceeded` lines in the first 300 ms.

If `enter_received` arrives late while `capute.begin` was immediate, the loss
is on Wi-Fi/capture activation. If `first_input` is late after `armed`, the
receiver/injection path is the bottleneck. If gaps only appear after the first
40 ms burst, the steady-state batching/network is the bottleneck.

## Baseline observed (2026-11-11, Wi-Fi)

Captured on the Ubuntu receiver while a Windows peer was the controller:

- `receiver.enter_received → receiver.armed → receiver.ack_sent` happened in
  the same millisecond, so the listener/arming path is not the bottleneck;
- `receiver.first_input` was **5–12 ms** after arming;
- during movement, the median gap between injected inputs was **~8.5 ms**
  (roughly 125 Hz), with outliers of **20–65 ms** throughout the session.

That points at the sender's input cadence/network delivery (and Wi-Fi jitter)
rather than capture activation on the receiver. The sender-side fixes in this
branch (40 ms handoff burst, Windows DSCP EF marking) target exactly that.
