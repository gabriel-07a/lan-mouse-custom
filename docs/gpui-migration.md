# GPUI migration status

Lan Mouse now ships the **GPUI frontend as the default UI** on Linux and
Windows. The legacy GTK/libadwaita frontend remains available as an opt-in
fallback (`--features gtk`) while parity work and release validation finish.

## Architecture

The service/daemon is unchanged. The UI is a **separate process**:

```
lan-mouse (service + IPC)  <->  lan-mouse-gpui (GPUI UI)
```

`lan-mouse` starts/reuses the service, spawns the sibling `lan-mouse-gpui`
binary and shuts the service down when the UI exits. The frontend talks to the
service over the existing local IPC protocol.

This split is intentional: the workspace's other dependencies unify zbus/tokio
features in a way that made a linked GPUI library panic in tray/portal workers.
Running the UI in its own process keeps its zbus/ksni feature resolution clean,
keeps the tray working and isolates UI crashes from the input service.

## Implemented

- Device dashboard with live state, connect/disconnect, position cycling,
  sensitivity, DNS, edit and remove.
- This Computer card with hostname, IPv4 addresses, port, fingerprint and
  copy-address/copy-fingerprint actions.
- Add-device dialog (hostname/IP, port, optional fingerprint).
- Pending connection requests with SAS and permission presets
  (both/control me/I control it/deny).
- Network scan and one-click add of discovered IPv4 peers.
- Settings: capture/emulation recovery, clipboard toggle, pairing window,
  authorized certificates (add/remove/rename), address rules (add/remove),
  per-device permissions and revoke.
- Linux StatusNotifierItem tray and Windows `tray-icon` tray prototype.
- Close-to-tray minimizes the window; tray "Open" reactivates it.
- Build/package plumbing for the standalone frontend (Linux `.deb`/tar and
  Windows zip/installer, Nix package).

## Verification

| Item | Status |
| --- | --- |
| Linux release build + window + IPC | verified locally |
| Linux fresh service + frontend startup | verified locally (isolated XDG runtime) |
| Linux tray registration | verified via `StatusNotifierItem` D-Bus |
| Linux `.deb` contains both binaries | verified locally |
| GPUI frontend unit tests | `cargo test` in `lan-mouse-gpui` |
| Windows MSVC frontend compile | verified in the earlier GPUI spike workflow |
| Windows GNU cross-build (main + frontend) | verified locally with `cargo zigbuild --target x86_64-pc-windows-gnu` |
| Windows release main + frontend package | workflow prepared; needs a manual CI run |
| Nix package | updated; needs a Nix evaluation/build |
| Performance | GPUI release frontend ~142 MB RSS in a debug-ish isolated run; release profile now uses LTO/strip |

## Known gaps / next steps

- Manual Windows release validation (manual `Release` workflow) and a real
  Windows run: installer, startup, tray, firewall, IPC.
- Nix flake evaluation/build on Linux.
- GTK fallback still in the workspace; remove it once the Windows/Linux
  matrix is signed off.
- Advanced client fields still GTK-only (enter/leave hooks, fix-IP list); the
  GPUI dialog covers hostname/port/fingerprint.
- Screenshot refresh for README/docs.

## Running the frontends

```sh
# GPUI (default)
cargo build --release
cargo build --release --manifest-path lan-mouse-gpui/Cargo.toml
./target/release/lan-mouse

# GTK fallback
cargo run --no-default-features --features "gtk,layer_shell_capture,x11_capture,libei_capture,wlroots_emulation,libei_emulation,rdp_emulation,x11_emulation"
```

The main binary looks for `lan-mouse-gpui` next to itself; set
`LAN_MOUSE_GPUI=/path/to/lan-mouse-gpui` while developing.
