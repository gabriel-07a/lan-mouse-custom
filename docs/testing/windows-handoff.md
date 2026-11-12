# Windows host return-coordinate validation

The return coordinate is platform-independent at the protocol level; Windows
has an additional local step (`SetCursorPos` after releasing the hidden-pointer
overlay). This matrix is the minimum manual validation for Windows 10/11.

## Automated coverage

- `lan-mouse-proto`: `LeaveAt` encode/decode and capability.
- `input-capture/src/windows/display_util.rs`: `point_for_edge_fraction`
  (right/bottom outer edge, multi-monitor) — runs in Windows CI.
- `input-emulation/src/absolute.rs`: `edge_along` (Linux and Windows CI).
- `input-capture/src/windows/event_thread.rs`: stale `LeaveAt` retries do not
  move the cursor; return fraction maps to the outer desktop edge.
- `src/service.rs`: return-coordinate retry preservation and capability
  lifetime.

## Manual matrix

For each Windows version (10 and 11), with Ubuntu as the client:

1. Same release on both machines.
2. Cross from Windows to Ubuntu through the right edge at 20%, 50% and 80% of
   the edge height.
3. Return through the same edge at a different height.
4. Confirm the Windows cursor reappears at the exit height, not the forward
   entry height.
5. Repeat 10 times and check for a stale `LeaveAt` retry warping the cursor
   after the capture was already released.
6. Repeat with two monitors and mixed DPI.

Expected trace on the Linux side:
`handoff: return request ... supported=true along=Some(...)`.
