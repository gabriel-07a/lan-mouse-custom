# Clipboard Ctrl+C/Ctrl+V: OS verification matrix (Windows 10/11 ↔ Ubuntu)

Manual acceptance matrix for cross-machine clipboard synchronization. The
automated layer (`tests/qa_clipboard_atomicity.rs`, `scripts/qa-clipboard-stress.sh`)
covers the transport/state machine deterministically; this document covers the
OS clipboard backends and network paths that can only be validated on real
machines (L5 in [docs/testing/README.md](README.md)).

Related docs: [docs/qa/clipboard.md](../qa/clipboard.md) (GNOME portal
`Invalid state` incident), [DOC.md](../../DOC.md#clipboard) (backend overview),
[docs/testing/clipboard-verification.md](clipboard-verification.md)
(automated evidence and residual risks for the atomicity fix).

## 1. What is automated vs manual

| Layer | Covered by | Runs where |
|---|---|---|
| Wire codec (frames 12–15, chunking, ack bitmap, limits) | `tests/qa_clipboard_atomicity.rs`, `lan-mouse-proto` unit tests | any host, `cargo test` |
| Sync state machine (offer/chunk/ack/cancel, retransmission, echo suppression, limits) | `tests/qa_clipboard_atomicity.rs`, `input-clipboard` unit tests | any host, `cargo test` |
| Local change → DTLS → listener → `write()` end to end, in memory | `tests/qa_clipboard_atomicity.rs` (harness mirrors `src/clipboard.rs` glue) | any host |
| Real DTLS/UDP loopback with clipboard frames | `src/clipboard.rs` test `clipboard_frames_traverse_dtls_in_both_directions` | any host |
| Literal OS clipboard (Wayland portal, X11, Windows API), secure desktop, RDP, UAC, Wi-Fi/VPN | **this matrix, manual** | real machines |
| Two full app instances on one Linux host | `scripts/qa-two-instances.sh` (input pipeline; clipboard needs a display) | Linux desktop |

Commands for the automated layer:

```sh
cargo test --test qa_clipboard_atomicity
cargo test --test qa_clipboard_atomicity -- --ignored   # residual/robustness cases
scripts/qa-clipboard-stress.sh --runs 20 --with-unit    # flake hunt
```

## 2. Prerequisites on both machines

1. **Same release** on both machines (from the same git commit/tag). Peers
   without the `CLIPBOARD` capability bit (older releases, `proto_version` 0
   peers) still get pointer/keyboard but **no clipboard sync**; verify input
   keeps working and no retry/error loop appears (S16).
2. **Clipboard enabled**: `[clipboard] enabled = true` (default) in
   `config.toml`, or start without `--no-clipboard`. Toggle at runtime with
   `lan-mouse cli clipboard enable|disable`.
3. **Permissions per direction**: the peer must be allowed `clipboard`
   (`--incoming`/`--outgoing` on `allow`/`pair`/`trust`, or via the GUI). The
   outgoing direction needs `clipboard-read` on the peer grant; the inbound
   direction needs `clipboard-write`. Host/client role does not change the
   direction semantics: *copy side sends, paste side receives*.
4. **Network**: IPv4 UDP 4242 for DTLS events and clipboard frames; UDP 4245
   for discovery. Firewall exceptions on the Windows side for both UDP ports.
   Weak Wi-Fi/VPN is fine for correctness but exercises retransmission (S18).
5. **Backend available**: `lan-mouse cli clipboard status` must show
   `backend: <name>`, `available: true`. `last error:` must be empty.
6. **Debug logs** (evidence): start the app with `LAN_MOUSE_LOG_LEVEL=debug`
   and capture stderr to a file (Windows: run `lan-mouse.exe` from a terminal so
   the parent console is reattached; Linux: `LAN_MOUSE_LOG_LEVEL=debug lan-mouse
   2> clipboard-debug.log`).
7. Both machines awake and unlocked for the row's duration. The Windows secure
   desktop (UAC prompt, lock screen, Ctrl+Alt+Del) is a known limitation, not a
   failure (see §7).

## 3. Backend selection matrix

| OS / session | Backend (`status`) | Notes |
|---|---|---|
| Windows 10/11, interactive user session | `windows` | `CF_UNICODETEXT`, `CF_HTML`, PNG via `CF_DIB`; no clipboard history/cloud sync |
| Ubuntu, Wayland (GNOME 50+/KDE) | `portal` | `org.freedesktop.portal.Clipboard`; one-time consent, persisted with a restore token; requires xdg-desktop-portal RemoteDesktop ≥ 2 |
| Ubuntu, Wayland (wlroots: Sway/Hyprland/…) | `wayland` | `wlr-data-control`, no consent dialog |
| Ubuntu, X11 | `x11` | `CLIPBOARD` selection with INCR for large payloads; XFixes owner-change (poll fallback) |
| Ubuntu, no display / headless | `none` (unavailable) | service still starts; clipboard sync stays disabled with `last_error` set — expected |

Backends are chosen when the clipboard task starts (and re-created after a
backend failure); a Wayland→X11 session switch requires an app restart.

## 4. Manual test matrix

`A → B` means **copy on A, paste on B**. Run every row in both replication
directions and both connection roles (Ubuntu configured as the dialing host,
Windows configured as the host). "Auto" links the deterministic automated
analogue in `tests/qa_clipboard_atomicity.rs`.

| ID | Scenario | Ubuntu → Windows | Windows → Ubuntu | Pass criteria | Auto |
|---|---|---|---|---|---|
| S1 | Plain ASCII text | ☐ | ☐ | Exact paste, one `applied` increment on the paste side | `text_copy_reaches_peer_exactly` |
| S2 | UTF-8 (accents/emoji), multiline, tabs | ☐ | ☐ | Byte-exact paste including newlines/tabs | `text_copy_reaches_peer_exactly` |
| S3 | 50 rapid copies, then paste | ☐ | ☐ | Last copied value always wins; no paste of an older value afterwards | `rapid_copies_never_lose_the_last_copy_under_loss` |
| S4 | Copy **before** the connection is up, then connect and paste | ☐ | ☐ | The pre-connection copy arrives once connected (no second Ctrl+C) | `copy_before_peer_registration_is_offered_on_connect` |
| S5 | First copy **immediately after** connect (<1 s) | ☐ | ☐ | Arrives without a second Ctrl+C (reported symptom) | `first_copy_after_connect_survives_a_lost_offer` |
| S6 | Copy during reconnect: kill the link (Wi-Fi off/on, peer app restart), copy while disconnected, then reconnect | ☐ | ☐ | After reconnect the clipboard resyncs; final paste matches the last copy (no stale content) | `reconnect_after_an_interrupted_transfer_resyncs_the_current_clipboard` |
| S7 | 50 distinct copies while connected (one paste each) | ☐ | ☐ | Every paste matches the copy that preceded it; no older value is ever applied after a newer one | `repeated_copies_are_applied_in_order_on_a_clean_link` |
| S8 | Large text ≈ 250 KiB (< default 256 KiB) | ☐ | ☐ | Paste exact; debug log shows chunked transfer with retransmissions finishing | `large_text_at_the_default_limit_transfers_exactly` |
| S9 | Oversized text > 256 KiB / image > 8 MiB | ☐ | ☐ | Copy is **not** synced; the copy side reports a size-limit error; the app stays responsive and a later small copy works | `oversized_local_snapshot_is_rejected_without_poisoning_the_session`, `inbound_oversized_offer_is_rejected_with_an_error` |
| S10 | HTML rich text (browser → word processor) | ☐ | ☐ | Formatted paste (or plain text fallback), no crash | `multi_format_snapshot_transfers_byte_exact` |
| S11 | PNG image (screenshot → image editor) | ☐ | ☐ | Image pastes intact | `multi_format_snapshot_transfers_byte_exact` |
| S12 | Disable clipboard sync, copy, enable, paste | ☐ | ☐ | No sync while disabled; the current clipboard is offered after re-enable | `copy_while_disabled_is_offered_after_re_enable` |
| S13 | Backend restart: Wayland restart `xdg-desktop-portal`/re-login, or restart the app | ☐ | ☐ | Clipboard resumes; current clipboard resyncs; no repeated permission prompts | `clipboard_survives_a_backend_restart_and_resyncs` |
| S14 | Disconnect mid-transfer (large copy, cut the network for ~2 s, restore) | ☐ | ☐ | Paste after reconnect shows the copied value; no stale offer left behind | `superseded_transfer_never_overwrites_a_newer_copy`, `incomplete_incoming_transfer_times_out_without_applying` |
| S15 | Second Ctrl+C after a lost/failed copy | ☐ | ☐ | The repeated Ctrl+C always re-offers the identical content (reported workaround) | `copying_again_after_reconnect_is_offered_again`, `pressing_copy_again_after_a_failed_transfer_repairs_delivery` |
| S16 | Echo/loop check: A copy → B paste → wait 5 s → copy on B → A paste | ☐ | ☐ | `received`/`detected` counters do not keep growing while idle; no clipboard flicker; no `applied` storm | `applied_remote_content_is_not_echoed_back` |
| S17 | Old peer without clipboard capability (e.g. previous release) | ☐ | ☐ | Pointer/keyboard keep working, no clipboard frames sent, no error loop | wire capability gate + `peer_supports(CLIPBOARD)` |
| S18 | Permission gate: grant only `clipboard-write` inbound / only `clipboard-read` outgoing, then swap | ☐ | ☐ | Only the granted direction transfers; denied direction is dropped with a debug log, never partially applied | `Sink::peer_may_read` + `listen` clipboard policy |
| S19 | Lossy network (weak Wi-Fi, VPN, `tc netem loss 5%` on the Linux side) during S3/S8 | ☐ | ☐ | Correct final clipboard, bounded retransmission, transfer aborts cleanly if the peer disappears | `rapid_copies_never_lose_the_last_copy_under_loss`, `rapid_copies…` |
| S20 | Clear clipboard on the copy side, then paste on the other | ☐ | ☐ | No empty frame is sent; the peer keeps its previous content (documented behavior) | `empty_snapshot_is_not_synced` |

## 5. Manual procedure

### 5.1 Common setup and evidence

On each machine, before the run:

```sh
lan-mouse cli clipboard status   # expect: enabled: true / available: true
lan-mouse cli devices            # confirm clipboard is in the granted permissions
```

Start the app with debug logging in one terminal (keep it running):

```sh
# Linux
LAN_MOUSE_LOG_LEVEL=debug lan-mouse 2> clipboard-debug.log
# Windows (PowerShell, from the install dir)
$env:LAN_MOUSE_LOG_LEVEL="debug"; .\lan-mouse.exe 2> clipboard-debug.log
```

For every row capture on the paste side, before and after:

```sh
lan-mouse cli clipboard status
# received:  N snapshot(s) — must increment by exactly 1 when the row passes
# detected:  M local change(s) — increments on the copy side for each Ctrl+C
# last error: - (empty)
```

Useful debug log lines (`grep -i clipboard clipboard-debug.log`):

- `using clipboard backend: <portal|windows|wayland|x11>`
- `clipboard: local change detected (N bytes, [...])`
- `clipboard: syncing N bytes in M format(s) to K peer(s)`
- `clipboard: applied N bytes in M format(s) from a peer`
- `clipboard: client <handle> speaks the clipboard extension, syncing with it`
- `clipboard: local change with no peers, not offering it` (S4: expected before connect)
- `clipboard: ignoring a duplicate change event` (compositor MIME updates)
- `clipboard: transfer N to peer X failed` / `clipboard: incoming transfer from peer X timed out`
- `clipboard: rejecting offer: ...` (limits), `clipboard: could not apply remote snapshot: ...`

On Linux, independently verify the OS clipboard contents:

```sh
# Wayland
wl-paste --no-newline ; wl-copy "probe"
# X11
xclip -selection clipboard -o ; xclip -selection clipboard <<< "probe"
```

On Windows: `Get-Clipboard` / `Set-Clipboard "probe"` in PowerShell, and paste
into Notepad (text), a browser contenteditable or WordPad (HTML), and Paint
(PNG) for S10/S11.

### 5.2 Scenario recipes

**S3/S7 rapid copies (Linux → Windows):**

```sh
for i in $(seq 1 50); do printf 'copy %s' "$i" | wl-copy; sleep 0.05; done
printf 'FINAL' | wl-copy
```

Then paste on Windows; result must be `FINAL` (and in S7 every intermediate
paste must match the value copied just before it). From Windows:

```powershell
1..50 | ForEach-Object { Set-Clipboard "copy $_"; Start-Sleep -Milliseconds 50 }
Set-Clipboard "FINAL"
```

**S4 copy before connect:** with the link down (peer app stopped, or the peer
disabled), copy; then start the peer / re-activate the client and cross the
edge; paste on the peer.

**S6/S14 reconnect:** configure a client, copy something (baseline), then cut
the network (disable Wi-Fi / `sudo ip link set <if> down` on Linux / disable
the adapter on Windows), copy again while the peer is unreachable, restore the
network and wait for the connection; paste on the peer. Repeat with a large
copy interrupted mid-transfer.

**S12 disable/enable:** `lan-mouse cli clipboard disable`, copy, paste on the
peer (must fail / keep old content), then `lan-mouse cli clipboard enable` and
paste again (the copy made while disabled is offered once sync is re-enabled).

**S18 loss:** add 5% loss on the Linux side and repeat S3/S7:

```sh
sudo tc qdisc add dev <iface> root netem loss 5%
# … run the scenario …
sudo tc qdisc del dev <iface> root
```

## 6. Sign-off template

Copy this table into the verification PR/issue and attach the two debug logs
and `clipboard status` outputs.

| Field | Value |
|---|---|
| Date / tester | |
| App version / git commit (both machines) | |
| Ubuntu version / session (Wayland GNOME, Wayland wlroots, X11) | |
| Ubuntu backend (`status`) | |
| Windows version/build (10 or 11) | |
| Windows backend (`status`) | |
| Connection roles tested (Ubuntu host / Windows host) | |
| S1–S20 result per direction (pass/fail) | |
| Failing rows + log excerpt + `last_error` | |
| Known limitations observed (§7) | |

Acceptance: every S1–S20 row passes in both replication directions and both
connection roles, except the documented limitations in §7. A row marked failed
must include the debug log excerpt and the exact `clipboard status` counters.

## 7. Known limitations and residual risks

These are expected behavior, not regressions:

- **Windows secure desktop**: UAC prompts, the lock screen and Ctrl+Alt+Del
  run on a different desktop; clipboard reads/writes during that time are
  unavailable and may be skipped. Do not count this as a clipboard failure.
- **RDP / clipboard redirection**: `rdpclip.exe` owns the clipboard inside an
  RDP session; test only in a local console session and disable clipboard
  redirection for reproducible results.
- **Windows clipboard history / cloud clipboard is not synchronized** by
  Lan Mouse; only the current clipboard snapshot is.
- **Wayland consent**: the first portal clipboard request shows a permission
  dialog; a denied/expired token keeps `available: false` with an actionable
  error. GNOME portal `Invalid state` after a portal update is a known stale
  portal bug — restart `xdg-desktop-portal`/re-login (see
  [docs/qa/clipboard.md](../qa/clipboard.md)).
- **Headless Linux**: no clipboard backend; sync stays disabled with an error
  in `status` (expected).
- **Eager snapshots**: the copy is serialized at Ctrl+C time (text, HTML, PNG
  only). Very large or unusual formats (files, TIFF, vendor MIME types) are
  not transferred; size caps (`[clipboard] max_*_bytes`) reject oversized
  snapshots and report the error in `status`.
- **Best-effort delivery budget**: the sender retransmits an unacknowledged
  window for ~1.2 s, then aborts and reports `last error: clipboard transfer
  aborted`. A silent transport outage longer than that needs a new copy or a
  reconnect event to resync today; content copied during a *detected*
  disconnect/reconnect is resynced automatically.
- **Ordered transport assumption**: the frame layer relies on DTLS delivering
  the association's datagrams in order (lossy but ordered). A transport that
  reorders application frames could let a superseded transfer's offer be
  accepted again; frames from peers that already disconnected are not
  currently rejected by peer identity (tracked as residual risks in
  [docs/testing/clipboard-verification.md](clipboard-verification.md)).
- **3+ machine topologies**: echo suppression is content-hash based; re-sending
  content that was most recently *received* from one peer to a third peer can
  be suppressed as an echo. The supported/verified topology is two machines.
- **macOS** support is dormant and not shipped; do not include it in the
  matrix.
