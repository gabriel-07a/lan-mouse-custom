# General Software Architecture

## Events

Each instance of lan-mouse can emit and receive events, where
an event is either a mouse or keyboard event for now.

The general Architecture is shown in the following flow chart:
```mermaid
graph TD
    A[Wayland Backend] -->|WaylandEvent| D{Input}
    B[X11 Backend] -->|X11Event| D{Input}
    C[Windows Backend] -->|WindowsEvent| D{Input}
    D -->|Abstract Event| E[Emitter]
    E -->|Udp Event| F[Receiver]
    F -->|Abstract Event| G{Dispatcher}
    G -->|Wayland Event| H[Wayland Backend]
    G -->|X11 Event| I[X11 Backend]
    G -->|Windows Event| J[Windows Backend]
```

### Input
The input component is responsible for translating inputs from a given backend
to a standardized format and passing them to the event emitter.

### Emitter
The event emitter serializes events and sends them over the network
to the correct client.

### Receiver
The receiver receives events over the network and deserializes them into
the standardized event format.

### Dispatcher
The dispatcher component takes events from the event receiver and passes them
to the correct backend corresponding to the type of client.


## Requests

// TODO this currently works differently

Aside from events, requests can be sent via a simple protocol.
For this, a simple tcp server is listening on the same port as the udp
event receiver and accepts requests for connecting to a device or to
request the keymap of a device.

```mermaid
sequenceDiagram
    Alice->>+Bob: Request Connection (secret)
    Bob-->>-Alice: Ack (Keyboard Layout)
```

## Problems
The general Idea is to have a bidirectional connection by default, meaning
any connected device can not only receive events but also send events back.

This way when connecting e.g. a PC to a Laptop, either device can be used
to control the other.

It needs to be ensured, that whenever a device is controlled the controlled
device does not transmit the events back to the original sender.
Otherwise events are multiplied and either one of the instances crashes.

To keep the implementation of input backends simple this needs to be handled
on the server level.

## Device State - Active and Inactive
To solve this problem, each device can be in exactly two states:

Either events are sent or received.

This ensures that
- a) Events can never result in a feedback loop.
- b) As soon as a virtual input enters another client, lan-mouse will stop receiving events,
which ensures clients can only be controlled directly and not indirectly through other clients.

## Clipboard
The clipboard is a separate subsystem (crate `input-clipboard`). A snapshot is
a set of canonical formats (`text/plain;charset=utf-8`, `text/html`,
`image/png`) and is transferred over the existing DTLS association as its own
frame type (12–15) with chunking, acknowledgements and retransmission. The sync
layer suppresses echoes by content hash.

OS integration:
- GNOME 50+ / Wayland: `org.freedesktop.portal.Clipboard` (requires a one-time
  consent, persisted with a restore token).
- Windows: `CF_UNICODETEXT`, `CF_HTML`, `PNG`/`CF_DIB`.
- macOS: `NSPasteboard` (`public.utf8-plain-text`, `public.html`, `public.png`).
- wlroots/X11: not implemented yet.

## Symmetric roles
Every instance runs both a listener and an outgoing connection, so *server* and
*client* above only describe who initiates a given DTLS session — not a fixed
role. With a client entry on each side, either device can control the other.

The handoff is coordinated with a serial number: the side that captures sends
`Enter` with a monotonically increasing serial and retransmits it until the
peer answers with `Ack(serial)`. The receiver deduplicates retransmissions and
releases its own capture exactly once. `Leave` returns control.

## Multi-OS Testing Strategy
Testing combinations across Linux, Windows 10, and Windows 11 uses three layers:
1. **Cross-platform geometry & protocol tests (`cargo test --workspace`):**
   - `input-emulation/src/absolute.rs`: virtual desktop bounding box, clamping, `0..=65535` normalization, and DPI logical/physical conversion.
   - `input-capture/src/windows/display_util.rs`: multi-monitor barrier crossing and origin clamping.
   - `src/connect.rs`: full end-to-end loopback tests (`Capture` + `DummyInputCapture` → DTLS → `LanMouseListener` → `Emulation` + `ClipboardFrame`).
2. **Real Windows kernel & session CI (`.github/workflows/rust.yml` and `release.yml`):**
   - Runs on both `windows-2022` (Windows 10 / Server 2022 kernel) and `windows-2025` (Windows 11 24H2 / Server 2025 kernel) using MSYS2 `UCRT64`.
   - `tests/windows_input.rs` exercises real `SendInput`, `GetCursorPos`, `WH_MOUSE_LL`, and `WH_KEYBOARD_LL` hooks on the runner's desktop session.
   - `release.yml` runs a silent install (`/S`) of the NSIS installer, verifies `lan-mouse.exe doctor --probe --json` reports `windows` for both capture and emulation, and runs the silent uninstaller.
3. **Local KVM/QEMU lab (`scripts/vm-test-lab.sh`):**
   - Generates `quickemu`/QEMU configurations for Windows 10 and Windows 11 (with virtual TPM 2.0, Secure Boot, and QXL multi-head virtual monitors for mixed-DPI testing) plus pre-paired `config.toml` files using `trusted_ips`.


