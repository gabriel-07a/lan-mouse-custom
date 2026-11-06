# Lan Mouse

> [!NOTE]
> **Unofficial fork.** This is a modified version of
> [Lan Mouse](https://github.com/feschber/lan-mouse) by its original
> contributors, licensed under GPL-3.0-or-later. Modifications © 2026
> Gabriel Arcanjo, made on 2026-11-03. See [NOTICE](NOTICE) for details.
> This fork is not endorsed by the upstream project.

[![CI](https://github.com/gabriel-07a/lan-mouse-custom/actions/workflows/rust.yml/badge.svg)](https://github.com/gabriel-07a/lan-mouse-custom/actions/workflows/rust.yml) [![Cachix](https://github.com/gabriel-07a/lan-mouse-custom/actions/workflows/cachix.yml/badge.svg)](https://github.com/gabriel-07a/lan-mouse-custom/actions/workflows/cachix.yml) [![Release](https://github.com/gabriel-07a/lan-mouse-custom/actions/workflows/release.yml/badge.svg)](https://github.com/gabriel-07a/lan-mouse-custom/actions/workflows/release.yml)

[![license](https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg)](LICENSE)

Lan Mouse is a *cross-platform* mouse and keyboard sharing software similar to universal-control on Apple devices.
It allows for using multiple PCs via a single set of mouse and keyboard.
This is also known as a Software KVM switch.

Goal of this project is to be an open-source alternative to proprietary tools like [Synergy 2/3](https://symless.com/synergy), [Share Mouse](https://www.sharemouse.com/de/)
and other open source tools like [Deskflow](https://github.com/deskflow/deskflow) or [Input Leap](https://github.com/input-leap) (Synergy fork).

Focus lies on performance, ease of use and a maintainable implementation that can be expanded to support additional backends for e.g. Android, iOS, ... in the future.

***blazingly fast™*** because it's written in rust.

- _Now with a gtk frontend_

<picture>
    <source media="(prefers-color-scheme: dark)" srcset="/screenshots/dark.png?raw=true">
    <source media="(prefers-color-scheme: light)" srcset="/screenshots/light.png?raw=true">
    <img alt="Screenshot of Lan-Mouse" srcset="/screenshots/dark.png">
</picture>


## Encryption

Lan Mouse encrypts all network traffic using the DTLS implementation provided by [WebRTC.rs](https://github.com/webrtc-rs/webrtc).
There are currently no mitigations in place for timing side-channel attacks.

## OS Support

Most current desktop environments and operating systems are fully supported, this includes
- GNOME >= 45
- KDE Plasma >= 6.1
- Most wlroots based compositors, including Sway (>= 1.8), Hyprland and Wayfire
- X11
- Windows
- MacOS


### Caveats / Known Issues

> [!Important]
> - **X11**: input capture and emulation are supported. Clipboard support on X11 covers text, HTML and small PNG images; payloads that require the X11 INCR protocol are skipped (not corrupted) and large X11 clipboard entries are not synchronized.
>
> - **Sway / wlroots**: Wlroots based compositors without libei support on the receiving end currently do not handle modifier events on the client side.
> This results in CTRL / SHIFT / ALT / SUPER keys not working with a sending device that is NOT using the `layer-shell` backend
>
> - **Wayfire**: If you are using [Wayfire](https://github.com/WayfireWM/wayfire), make sure to use a recent version (must be newer than October 23rd) and **add `shortcuts-inhibit` to the list of plugins in your wayfire config!**
> Otherwise input capture will not work.
>
> - **Windows**: The mouse cursor will be invisible when sending input to a Windows system if
> there is no real mouse connected to the machine.
>
> - **Windows pointer at the screen edge (10 and 11)**: pointer motion and barrier crossings
> come from raw input (`WM_INPUT`), not from the low-level hook's clamped cursor position:
> Windows stops the cursor at the edge pixel, so an "out of bounds" hook coordinate is not
> reliable across OS versions and DPI scales. Raw deltas are also what drives motion while a
> peer is controlled, and injected UI events (`SendInput`) are never mistaken for a local edge
> crossing. If a pointer still sticks at an edge, run with `LAN_MOUSE_LOG_LEVEL=debug` and check
> for the `ENTERED @ ...` line: without it the edge is not being treated as a barrier (usually
> a missing client at that position or a display layout where that edge is not the outer one).
>
> - **Local cursor while sending**: the pointer of the machine that currently controls a peer
> is hidden, so the cursor appears to "move" to the other screen instead of sitting parked at
> the edge. Windows, macOS and wlroots/layer-shell backends support this; on GNOME/Wayland the
> pointer is compositor-owned (libei has no hide API), so it stays visible there. The cursor is
> restored on every release/Leave/disconnect path, and a crash cannot leave it hidden (the
> Windows overlay window is destroyed with the process).

For more detailed information about os support see [Detailed OS Support](#detailed-os-support)

### Android & IOS

A proof of concept for an Android / IOS Application by [rohitsangwan01](https://github.com/rohitsangwan01) can be found [here](https://github.com/rohitsangwan01/lan-mouse-mobile).
It can be used as a remote control for any device supported by Lan Mouse.

## Installation

<details>
    <summary>Arch Linux</summary>

Lan Mouse can be installed from the [official repositories](https://archlinux.org/packages/extra/x86_64/lan-mouse/):

```sh
pacman -S lan-mouse
```

The prerelease version (following `main`) is available on the AUR:

```sh
paru -S lan-mouse-git
```
</details>


<details>
    <summary>Nix (OS)</summary>

- nixpkgs: [search.nixos.org](https://search.nixos.org/packages?channel=unstable&show=lan-mouse&from=0&size=50&sort=relevance&type=packages&query=lan-mouse)
- flake: [README.md](./nix/README.md)
</details>

<details>
    <summary>Fedora</summary>
You can install Lan Mouse from the [Terra Repository](https://terra.fyralabs.com).


After enabling Terra:

```sh
dnf install lan-mouse
```
</details>

<details>
    <summary>MacOS</summary>

- Download the package for your Mac (Intel or ARM) from the releases page
- Unzip it
- Remove the quarantine with `xattr -rd com.apple.quarantine "Lan Mouse.app"`
- Launch the app
- Use the menu bar item to open the settings window or quit Lan Mouse. Bundled macOS builds run as a menu bar app and do not keep a Dock icon visible.
- Grant accessibility permissions in System Preferences

</details>

<details>
    <summary>Windows</summary>

Lan Mouse can be installed from the [winget community repositories](https://github.com/microsoft/winget-pkgs/tree/master/manifests/f/feschber/LanMouse):

```sh
winget install lan-mouse
```

The unofficial fork publishes its own installer
(`lan-mouse-custom-setup-x86_64.exe`) in the releases of this repository.
Its clean-install behavior is described in
[Clean install, reset and uninstall](#clean-install-reset-and-uninstall).

</details>

<details>
    <summary>Manual Installation</summary>

First make sure to [install the necessary dependencies](#installing-dependencies-for-development--compiling-from-source).

Precompiled release binaries for Windows, MacOS and Linux are available in the [releases section](https://github.com/feschber/lan-mouse/releases).
For Windows, the depenedencies are included in the .zip file, for other operating systems see [Installing Dependencies](#installing-dependencies-for-development--compiling-from-source).

Alternatively, the `lan-mouse` binary can be compiled from source (see below).

### Installing desktop file, app icon and firewall rules (optional)
```sh
# install lan-mouse (replace path/to/ with the correct path)
sudo cp path/to/lan-mouse /usr/local/bin/

# install app icon
sudo mkdir -p /usr/local/share/icons/hicolor/scalable/apps
sudo cp lan-mouse-gtk/resources/de.feschber.LanMouse.svg /usr/local/share/icons/hicolor/scalable/apps

# update icon cache
gtk-update-icon-cache /usr/local/share/icons/hicolor/

# install desktop entry
sudo mkdir -p /usr/local/share/applications
sudo cp de.feschber.LanMouse.desktop /usr/local/share/applications

# when using firewalld: install firewall rule
sudo cp firewall/lan-mouse.xml /etc/firewalld/services
# -> enable the service in firewalld settings
```

Instead of downloading from the releases, the `lan-mouse` binary
can be easily compiled via cargo or nix:

### Compiling and installing manually:
```sh
# compile in release mode
cargo build --release

# install lan-mouse
sudo cp target/release/lan-mouse /usr/local/bin/
```

### Compiling and installing via cargo:
```sh
# will end up in ~/.cargo/bin
cargo install lan-mouse
```

### Compiling and installing via nix:
```sh
# you can find the executable in result/bin/lan-mouse
nix-build
```
### Conditional compilation
Support for other platforms is omitted automatically based on the active
rust toolchain.

Additionally, available backends and frontends can be configured manually via
[cargo features](https://doc.rust-lang.org/cargo/reference/features.html).

E.g. if only support for sway is needed, the following command produces
an executable with support for only the `layer-shell` capture backend
and `wlroots` emulation backend:
```sh
cargo build --no-default-features --features layer_shell_capture,wlroots_emulation
```
For a detailed list of available features, checkout the [Cargo.toml](./Cargo.toml)
</details>



## Clean install, reset and uninstall

Lan Mouse stores everything it persists for one user (devices, permissions,
certificates, address rules, audit log, portal consent tokens) in directories
named `lan-mouse`. A **package upgrade keeps this data on purpose**; a **fresh
install** must not resurrect it. The exact paths per OS are listed in
[docs/audit-fresh-install.md](docs/audit-fresh-install.md).

### Windows installer

The Windows installer detects leftovers from older or portable builds in
`%LOCALAPPDATA%\lan-mouse` and `%APPDATA%\lan-mouse`. Cleaning is
**reversible**: the old directory is renamed to a timestamped sibling
`lan-mouse.bak-<time>` instead of being deleted, so the next start shows no old
PCs while the data can still be restored.

* **Interactive install**: asks before cleaning (default **Yes**, so installing
  a release starts fresh). Answer **No** to keep your devices and permissions
  when upgrading.
* **Silent install** (`setup.exe /S`): backs up the old configuration by
  default. Pass `/KEEPUSERDATA` to keep it in place, `/CLEANUSERDATA` to force
  the clean install, or `/PURGEUSERDATA` to delete it permanently.
* **Uninstall**: stops the GUI and its daemon and moves the same two
  directories to a backup (interactive asks, default **Yes**; silent uninstall
  moves them too). `/KEEPUSERDATA` keeps them in place; `/PURGEUSERDATA`
  deletes them permanently. Shortcuts, firewall rule and registry keys are
  removed.

Restore a backup by renaming it back, e.g. Explorer → rename
`%LOCALAPPDATA%\lan-mouse.bak-<time>` to `lan-mouse`, or in PowerShell:

```powershell
Rename-Item "$env:LOCALAPPDATA\lan-mouse.bak-<time>" "lan-mouse"
```

Delete old `lan-mouse.bak-*` directories manually when you no longer need
them. Quit Lan Mouse before installing/uninstalling manually; the installer
stops a running instance for you (`taskkill /T /F`).

### Reset (Linux, macOS and Windows)

```sh
lan-mouse --reset-config --dry-run   # show every path that would be moved
lan-mouse --reset-config             # move them to .bak backups (reversible)
lan-mouse --reset-config --purge     # delete them permanently
```

The default is reversible: every directory is renamed to a sibling
`lan-mouse.bak-<unix timestamp>` (e.g. `~/.config/lan-mouse.bak-1762370000`),
which hides the old devices from the next start. Restore it with
`mv ~/.config/lan-mouse.bak-<timestamp> ~/.config/lan-mouse` (or use
`--purge` if you are sure you never want the data back). The command only
touches directories literally named `lan-mouse` (config, state, legacy
portal-token cache, macOS audit logs), refuses to run while the service is
running, never follows symlinks and does not touch custom `--config` /
`--cert-path` locations. On Linux/macOS the helper script
`scripts/install-clean.sh --dry-run` / `--yes` / `--purge` does the same when
the binary is not installed anymore.

On Debian/Ubuntu (`apt`), package upgrades and even `apt purge` keep the
per-user data; the postrm explains the paths and the reset command. GNOME/KDE
will ask for clipboard/input-capture/remote-desktop consent again after a
reset. On macOS the Accessibility/Input Monitoring grants are managed by the OS
and are **not** reset by the command.

## Development

### Git pre-commit hook

This repository includes a local git hooks directory `.githooks/` with a `pre-commit` script that enforces formatting, lints, and tests before allowing a commit.  It is optional to enable it, but it will prevent you from committing code with failing unit tests or that needs clippy/fmt fixes. To enable the hook locally:

1. Make the hook executable:

```sh
chmod +x .githooks/pre-commit
```

2. Point git to the hooks directory (one-time per clone):

```sh
git config core.hooksPath .githooks
```

The `pre-commit` script runs `cargo fmt --all` (and fails if files were modified), `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features`.

### Dependencies & Compiling from Source
<details>
    <summary>MacOS</summary>

```sh
# Install dependencies
brew install libadwaita pkg-config imagemagick
cargo install cargo-bundle
# Create the macOS icon file
scripts/makeicns.sh
# Create the .app bundle
cargo bundle
# Copy all dynamic libraries into the bundle, and update the bundle to find them there
scripts/copy-macos-dylib.sh
```
</details>

<details>
    <summary>Ubuntu and derivatives</summary>

```sh
sudo apt install libadwaita-1-dev libgtk-4-dev libx11-dev libxtst-dev
```
</details>

<details>
    <summary>Arch and derivatives</summary>

```sh
sudo pacman -S libadwaita gtk libx11 libxtst
```
</details>

<details>
    <summary>Fedora and derivatives</summary>

```sh
sudo dnf install libadwaita-devel libXtst-devel libX11-devel
```
</details>
<details>
    <summary>Nix</summary>

```sh
nix-shell .
```
</details>
<details>
    <summary>Nix (flake)</summary>

```sh
nix develop
```
</details>

<details>
    <summary>Windows</summary>

- First install [Rust](https://www.rust-lang.org/tools/install).

- Then follow the instructions at [gtk-rs.org](https://gtk-rs.org/gtk4-rs/stable/latest/book/installation_windows.html)

*TLDR:*

Build gtk from source

- The following commands should be run in an **admin power shell** instance:
```sh
# install chocolatey
Set-ExecutionPolicy Bypass -Scope Process -Force; iex ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))

# install gvsbuild dependencies
choco install python git msys2 visualstudio2022-workload-vctools
```

- The following commands should be run in a **regular power shell** instance:

```sh
# install gvsbuild with python
python -m pip install --user pipx
python -m pipx ensurepath
```

- Relaunch your powershell instance so the changes in the environment are reflected.
```sh
pipx install gvsbuild

# build gtk + libadwaita
gvsbuild build gtk4 libadwaita librsvg adwaita-icon-theme
```

- **Make sure to add the directory** `C:\gtk-build\gtk\x64\release\bin`
[**to the `PATH` environment variable**]((https://learn.microsoft.com/en-us/previous-versions/office/developer/sharepoint-2010/ee537574(v=office.14))). Otherwise the project will fail to build.

To avoid building GTK from source, it is possible to disable
the gtk frontend (see conditional compilation).
</details>

## Usage
<details>
    <summary>Gtk Frontend</summary>

By default the gtk frontend will open when running `lan-mouse`.

To connect a device you want to control, simply click the `Add` button and enter the hostname
of the device. The **This Computer** section at the top of the window shows the single local IP
address the other computer should use (the one of the default route), the listening port, and a
button to copy the address — no terminal needed. The tooltip lists other local addresses (e.g. a
second NIC or IPv6) and their interfaces. Use the refresh button after connecting to another
network; the row also updates when the port changes.

On the *remote* device, authorize your *local* device for incoming traffic using the `Authorize` button
under the "Incoming Connections" section.
The fingerprint for authorization can be found under the general section of your *local* device.
It is of the form "aa:bb:cc:..."

Authorized devices can be persisted using the configuration file (see [Configuration](#configuration)).

If the device still can not be entered, make sure you have UDP port `4242` (or the one selected) opened up in your firewall.
</details>

<details>
    <summary>Command Line Interface</summary>

The cli interface can be accessed by passing `cli` as a commandline argument.
Use
```sh
lan-mouse cli help
```
 to list the available commands and
```sh
lan-mouse cli <cmd> help
```
for information on how to use a specific command.

</details>

<details>
    <summary>Daemon Mode</summary>

Lan Mouse can be launched in daemon mode to keep it running in the background (e.g. for use in a systemd-service).

To do so, use the `daemon` subcommand:

```sh
lan-mouse daemon
```
</details>

## Systemd Service

In order to start lan-mouse with a graphical session automatically,
the [systemd-service](service/lan-mouse.service) can be used:

Copy the file to `~/.config/systemd/user/` and enable the service:

```sh
cp service/lan-mouse.service ~/.config/systemd/user
systemctl --user daemon-reload
systemctl --user enable --now lan-mouse.service
```
> [!Important]
> Make sure to point `ExecStart=/usr/bin/lan-mouse daemon` to the actual `lan-mouse` binary (in case it is not under `/usr/bin`, e.g. when installed manually.


## Configuration
To automatically load clients on startup, the file `$XDG_CONFIG_HOME/lan-mouse/config.toml` is parsed.
`$XDG_CONFIG_HOME` defaults to `~/.config/`.

To create this file you can copy the following example config:

### Example config
> [!TIP]
> key symbols in the release bind are named according
> to their names in [input-event/src/scancode.rs#L172](input-event/src/scancode.rs#L176).
> This is bound to change

```toml
# example configuration

# configure release bind
release_bind = [ "KeyA", "KeyS", "KeyD", "KeyF" ]

# optional port (defaults to 4242)
port = 4242

# list of authorized tls certificate fingerprints that
# are accepted for incoming traffic
[authorized_fingerprints]
"bc:05:ab:7a:a4:de:88:8c:2f:92:ac:bc:b8:49:b8:24:0d:44:b3:e6:a4:ef:d7:0b:6c:69:6d:77:53:0b:14:80" = "iridium"

# define a client on the right side with host name "iridium"
[[clients]]
# position (left | right | top | bottom)
position = "right"
# hostname
hostname = "iridium"
# activate this client immediately when lan-mouse is started
activate_on_startup = true
# optional list of (known) ip addresses
ips = ["192.168.178.156"]

# define a client on the left side with IP address 192.168.178.189
[[clients]]
position = "left"
# The hostname is optional: When no hostname is specified,
# at least one ip address needs to be specified.
hostname = "thorium"
# ips for ethernet and wifi
ips = ["192.168.178.189", "192.168.178.172"]
# optional port
port = 4242
```

Where `left` can be either `left`, `right`, `top` or `bottom`.

Each client may also carry the peer's certificate fingerprint:

```toml
[[clients]]
position = "right"
ips = ["192.168.178.156"]
# pinned SHA-256 fingerprint of the peer certificate (optional)
fingerprint = "bc:05:ab:7a:a4:de:88:8c:2f:92:ac:bc:b8:49:b8:24:0d:44:b3:e6:a4:ef:d7:0b:6c:69:6d:77:53:0b:14:80"
```

With a pinned fingerprint the outgoing connection verifies the server
certificate and refuses a host that presents a different one (DHCP reuse, a
wrong machine on that IP, or a man-in-the-middle). Without a pin the first
successful connection learns and stores the fingerprint (trust on first use),
so later connections are authenticated; `lan-mouse cli set-fingerprint <id>`
can pin it explicitly and `set-fingerprint <id>` without a value clears it.

### Device permissions and control direction

Every authorized machine is a *device* keyed by its certificate fingerprint.
Each device has **two independent directions**, and each direction has its own
set of capabilities (`pointer`, `keyboard`, `clipboard`):

- **incoming** — what that device may do on *this* machine. It is enforced
  locally: keyboard/pointer/clipboard events that were not granted are dropped
  before they reach the emulation backend.
- **outgoing** — what *this* machine may do on the peer. It is a local guard:
  the machine refuses to activate the link (and stops forwarding input) when
  the direction or capability was not granted.

This covers all direction combinations: "only this PC controls the other",
"only the other controls this PC", "both control each other" (the default),
optionally with different capabilities per direction, e.g. "the other machine
may use the clipboard but not move my mouse".

The GUI has a **Devices** section with one expandable row per device and
checkboxes for each direction; the authorization dialog asks for the
capabilities of both directions when a new device is approved. The CLI has:

```sh
lan-mouse cli devices                  # name, fingerprint, position, caps, state
lan-mouse cli pending                 # devices waiting for approval
lan-mouse cli allow <fingerprint> --name windows11 --position right \
    --incoming pointer,keyboard,clipboard --outgoing pointer,keyboard
lan-mouse cli deny <fingerprint> [--for 1h]  # deny permanently or for a while
lan-mouse cli allow <fingerprint> --incoming keyboard --once  # session grant
lan-mouse cli revoke <fingerprint>    # close sessions + deny
lan-mouse cli rename <fingerprint> "desk pc"
```

`--once` grants access only for the current session: the record is never
written to the config and is revoked as soon as the peer disconnects, so the
device is asked again next time. `deny --for 1h` (or `30m`, `2d`) makes a
temporary revocation that stops blocking when it expires.

`--incoming`/`--outgoing` accept `pointer,keyboard,clipboard`, `all` or `none`;
clipboard can also be split into `clipboard-read` (the peer may read this
machine's clipboard) and `clipboard-write` (the peer may set it), while the
legacy `clipboard` value grants both. `none` disables that direction. Policies
are stored per device in the config:

```toml
[[devices]]
fingerprint = "bc:05:ab:…"
name = "windows11"

[devices.incoming]          # what the peer may do here
allowed = true
capabilities = 7            # 1=pointer, 2=keyboard, 4=clipboard

[devices.outgoing]          # what this PC may do there
allowed = true
capabilities = 7
```

Old configs are migrated on load: every entry in `authorized_fingerprints`
becomes a device with full access in both directions (the previous behavior),
so nothing changes until a policy is narrowed. A device can also be granted
*connect-only* access (`allowed = true`, `capabilities = 0`): it may stay
connected and answer pings but cannot inject any input.

### Auto-authorizing address rules

An address rule lets devices from an IP or CIDR connect without an approval
prompt. Unlike the old `trusted_ips` allowlist, a rule carries the permissions
the adopted device gets, so a whole subnet can be allowed to use the clipboard
but not to control the mouse or keyboard:

```toml
[[address_rules]]
address = "10.0.0.0/24"

[address_rules.incoming]    # what devices from this range may do here
allowed = true
capabilities = 4            # clipboard only

[address_rules.outgoing]    # what this PC may do there
allowed = true
capabilities = 0            # connect-only
```

```sh
lan-mouse cli trust 10.0.0.0/24 --incoming clipboard --outgoing none
lan-mouse cli trusted        # list rules and their permissions
lan-mouse cli untrust 10.0.0.0/24
```

Rules that cover more than a local subnet (for example `10.0.0.0/8` or
`0.0.0.0/0`) are accepted but logged and reported as a warning. The adopted
device still appears in the device list with the rule's permissions and can be
narrowed or revoked per device; `trusted_ips` from older configs is migrated to
full-access rules so existing setups keep working.

### Permission negotiation and verification code

Peers from this fork advertise a `PERMISSIONS` capability and exchange
`AuthRequest`/`AuthResponse` messages once connected, so each side learns the
capabilities the other grants it (shown in the client state) without changing
the enforcement model: grants are always enforced locally. Legacy peers never
receive these events (they would close the connection on an unknown id), so
interoperability is preserved.

When a new device asks for approval, the dialog/CLI shows a **six-digit
verification code** derived from both certificate fingerprints. Two machines
that know each other's fingerprint display the same code; compare it out of
band (screenshot, chat) before approving. If a known device later shows up with
a **different certificate** for the same address or name, the frontend warns
about the change, the request is flagged and the event is audited.

### Kiosk mode

Set `accept_incoming = false` to make this machine control-only: outgoing
links and clipboard sends keep working, but every incoming handshake is
rejected in the certificate callback, so authorized devices, address rules and
an open pairing window cannot connect either.

```toml
accept_incoming = false
```

### Policy backup and transfer

`export-policy` writes the persistent permission policy (devices, address
rules and revocations, without link addresses or session state) as JSON;
`import-policy` validates the whole file first and only then replaces the
policy, so a malformed file changes nothing.

```sh
lan-mouse cli export-policy permits.json
lan-mouse cli import-policy permits.json
```

Import and export are audited (`policy_exported`/`policy_imported`).

### Local IPC security

On Unix the frontend socket is protected twice: it lives in
`$XDG_RUNTIME_DIR` (0700) and the listener verifies the peer UID through
`SO_PEERCRED`, rejecting IPC clients running as another user. On Windows the
frontend uses a loopback TCP socket, which cannot carry peer credentials; any
local process that can connect to `127.0.0.1:5252` can talk to the service, so
do not run untrusted software in the same session. The private key file gets
`0400` permissions on Unix and a best-effort `icacls` restriction to the
current user on Windows (`RW` removed for everyone else).

### Connection limits

The listener bounds abuse from the LAN even before a certificate is accepted:
each IP may start at most 20 handshakes per minute (the connect side backs off
to 5 s, so normal reconnects stay well below that), at most 32 handshakes can
be in flight and at most 128 connections can be live at once. Rejected
handshakes are only remembered for 60 s, so a stream of random certificates
cannot grow the deduplication map forever. All limits are internal constants;
they can be revisited if a large deployment needs more headroom.

### Audit log

Every permission change is appended to a JSON-lines audit log at
`$XDG_STATE_HOME/lan-mouse/audit.jsonl` (`%LOCALAPPDATA%\lan-mouse\audit.jsonl`
on Windows, `~/Library/Logs/lan-mouse/audit.jsonl` on macOS), rotated at 5 MB
to `audit.jsonl.1`. It records grants (`grant_created`, `grant_updated`),
`revoke`, `deny`, pairing windows and adopted devices, address-rule changes and
TUF certificate pinning, with fingerprint, name, address and a short detail
(source, capabilities, reason). The log never contains input or clipboard
contents.

```sh
lan-mouse cli history --limit 20   # recent events, newest first
lan-mouse doctor                  # prints the audit path and a recent count
```

## Two-way (mutual) operation

Lan Mouse has **no fixed server/client role**: every instance listens for
incoming connections *and* dials the clients configured in its own config.
Either machine can therefore drive the other one — as long as both sides are
configured.

To let both machines control each other:

1. Run lan-mouse on **both** machines.
2. On each machine add the *other* one as a `[[clients]]` entry with the
   mirrored position:
   - on PC A: `position = "right"` for PC B,
   - on PC B: `position = "left"` for PC A.
3. Authorize each peer's certificate fingerprint on the receiving side
   (`authorized_fingerprints`, or approve the prompt in the frontend).
4. Allow the same UDP port (default `4242`) through the firewall on both
   machines.

Only one side holds the input capture at a time: the machine whose cursor
crossed the edge last takes control, and the other side releases its capture
so events can never loop back. If only one side has a client entry, control
works in one direction only.

`lan-mouse doctor` prints the configured clients, the selected capture and
emulation backends, the local fingerprint and warnings for common
misconfigurations. Add `--probe` to also initialize the input backends, and
`--json` for scripted use.

### Network tuning

- `motion_batch_ms` (default `4`, `0` disables): coalesces pointer motion into
  small batches. On Wi-Fi this cuts the motion packet rate by roughly 4x with
  less than 5 ms of added latency. Values above ~8 ms start to be noticeable.
- `connect_grace_ms` (default `400`, `0` disables): how long an input event may
  wait for a (re)connection before the capture is released. Keeps the mouse
  grabbed during a short Wi-Fi hiccup instead of bouncing back to the local
  screen immediately.

Both options also exist as command line flags (`--motion-batch-ms`,
`--connect-grace-ms`).

## Clipboard

Lan Mouse can synchronize the clipboard (plain text, HTML and PNG images)
between authorized, connected peers.

- Enabled by default. Toggle it in the frontend or with
  `lan-mouse-cli clipboard enable|disable`; `--no-clipboard` disables it at
  startup.
- Size limits are configurable under `[clipboard]` (defaults: 256 KiB text,
  1 MiB HTML, 8 MiB image, 10 MiB per snapshot).
- **Linux / Wayland (GNOME 50+)** uses the XDG desktop portal. The first run
  asks for **"Allow Clipboard Access"**; the consent is stored in
  `~/.cache/lan-mouse/clipboard.token` (delete it to be asked again).
- **Windows** and **macOS** use the native clipboard. On Windows, images are
  converted between the canonical PNG and `CF_DIB`/`CF_DIBV5`.
- Content marked as secret by the source application (macOS
  `org.nspasteboard.ConcealedType`, KDE password-manager hint, Windows
  clipboard-history/cloud opt-outs) is never synchronized.
- Clipboard sync only runs between authorized peers and stops immediately when
  the toggle is off.

```toml
[clipboard]
enabled = true
max_text_bytes = 262144
max_html_bytes = 1048576
max_image_bytes = 8388608
max_snapshot_bytes = 10485760
```

Clipboard backends: GNOME/KDE use the XDG portal, wlroots compositors use
`wlr-data-control`, and X11 uses the `CLIPBOARD` selection. The X11 backend
handles text, HTML and small PNG images; large payloads (INCR) are skipped.

### Pairing a new device

A device only has to be approved once. Publishing the fingerprint by hand is
not necessary:

1. On the machine you are sitting at, add a client with the other machine's
   address (GUI: *add client* → hostname/IP; CLI:
   `lan-mouse cli add-client --ips 192.168.1.50 --position left --activate`).
2. The other machine shows an **Unauthorized Device** window. Pick where this
   device sits on its screen and press **Authorize**.
3. That single approval stores the fingerprint *and* creates the client entry
   for the reverse direction, using the address of the accepted connection, so
   both machines can control each other without editing the config by hand.

From a terminal the approval can also be granted with

```sh
lan-mouse cli authorize-device <sha256-fingerprint> --position left
```

which has the same effect: the fingerprint is trusted and a client for the
peer is created and activated when it connects. A fingerprint that was
authorized this way is never asked for again unless the peer regenerates its
certificate.

### Sensitivity per device

Wayland reports raw pointer deltas while Windows and macOS report accelerated
ones, so the cursor speed differs between peer combinations. Each client has a
`sensitivity` (percent, `100` = unchanged) that scales the forwarded motion:

```toml
[[clients]]
hostname = "windows11"
ips = ["192.168.166.166"]
position = "right"
sensitivity = 120   # 20% faster
```

The GUI exposes it in the client row; the CLI has
`lan-mouse cli set-sensitivity <id> <percent>`. Fractional deltas from Wayland
are accumulated instead of truncated, so slow movements are not lost on the
integer-only Windows/X11 backends.

Motion is carried in logical (96 DPI) pixels. A Windows sender divides the
physical deltas reported by its hooks by the display scale, and a Windows
receiver multiplies them by the scale of the monitor under the cursor, so a
150% laptop and a 100% desktop move the pointer the same logical distance.
`sensitivity` is applied on top of that, so it only has to compensate for
pointer acceleration, not for display scaling. A scale change on the receiver
is picked up without a restart when the cursor moves to another monitor (the
per-monitor scale is cached for two seconds).

### Pairing without a mouse on the other machine

`lan-mouse cli pair --seconds 60 --incoming pointer,keyboard --outgoing none`
(or the *Pairing mode* button in the GUI, which pairs with full access) opens a
window during which **one** new connecting device is authorized with the
configured permissions and, unless the outgoing direction is disabled, also
gets a client entry for the reverse direction. Useful when the other computer
has no mouse to click "approve". The window is single use: it closes as soon
as the first device is accepted, and the frontend gets a **pairing review**
listing what was adopted during the window (so it can be narrowed or revoked).
After it closes, unknown devices need the normal approval again; already-seen
ones are matched by certificate.

Devices running this fork also announce themselves every 10 seconds with their
certificate fingerprint, so they show up in the network table immediately and
can be authorized in one click.

### Revoking a device

`lan-mouse cli remove-authorized-key <fingerprint>` (or the delete button in
the *Authorized Certificates* list) does more than forget the fingerprint: it
closes the live session, deactivates the outgoing link and adds the
fingerprint to a deny list (`denied_fingerprints` in the config), so an open
pairing window or a trusted address cannot silently re-authorize the same
certificate. Authorizing the device again explicitly removes it from the deny
list. Trusted addresses that cover more than a local subnet (for example
`10.0.0.0/8` or `0.0.0.0/0`) are accepted but logged and reported as a warning:
every certificate from that range is authorized without a prompt.

### GNOME asks once per login

On GNOME the `InputCapture` portal is still at **version 1**, which has no
`restore_token`/`persist_mode`: the compositor asks for permission every time
a capture session is created. This fork therefore creates exactly **one**
capture session per daemon start and keeps it for the whole session, so the
dialog appears once per login and never while you cross the screen edges.

Click **Share** (and enable the toggle) once. If you cancel, input capture
stays off until you re-enable it (`lan-mouse cli enable-capture`, or the
button in the GUI) — the daemon does not retry, so it will not spam the
dialog.

The emulation (`RemoteDesktop`, portal v2) and clipboard sessions store their
restore tokens under `~/.cache/lan-mouse/`, so they are only approved once.

### Peers running an upstream release

Peers advertise optional features in the `Pong` they send for the first `Ping`
of a connection. The trailing bytes are ignored by older releases, so the
negotiation itself can never break them. The `Hello` build handshake is only
sent once a peer has proved that it understands it: released versions before
`Hello` existed disconnect on an unknown event id, which used to turn the
outgoing connection into a write-only one that the peer never answered.

Everything this fork adds is negotiated:

| Feature | Peer without the extension |
| --- | --- |
| `Enter` serial + acknowledgement | The handoff is sent once, exactly like upstream. Nothing waits for an `Ack` that a legacy peer never sends. |
| Clipboard sync | No clipboard frame is offered, so a legacy peer never sees a frame it cannot decode. |
| Ping liveness | A connection whose peer never answers a ping is kept for 30 rounds (about a minute) instead of being torn down after two. Some peers read a connection without ever writing to it. |

Input sharing itself is wire-compatible in both directions, so mixed setups
(this fork on one machine, an upstream release on the other) work for keyboard
and mouse; only the features above degrade to upstream behaviour.

## Roadmap
- [x] Graphical frontend (gtk + libadwaita)
- [x] respect xdg-config-home for config file location.
- [x] IP Address switching
- [x] Liveness tracking Automatically ungrab mouse when client unreachable
- [x] Liveness tracking: Automatically release keys, when server offline
- [x] MacOS KeyCode Translation
- [x] Libei Input Capture
- [x] MacOS Input Capture
- [x] Windows Input Capture
- [x] Encryption
- [x] X11 Input Capture
- [x] Latency measurement and visualization (RTT per client)
- [ ] Bandwidth usage measurement and visualization
- [x] Clipboard support (text, HTML, PNG; portal + data-control + Windows/macOS/X11)


## Detailed OS Support

In order to use a device for sending events, an **input-capture** backend is required, while receiving events requires
a supported **input-emulation** *and* **input-capture** backend.

A suitable backend is chosen automatically based on the active desktop environment / compositor.

The following sections detail the emulation and capture backends provided by lan-mouse and their support in desktop environments / operating systems.

### Input Emulation Support

| Desktop / Backend         | wlroots                  | libei                    | remote-desktop portal    | windows                  |   macos                                | x11                |
|---------------------------|--------------------------|--------------------------|--------------------------|--------------------------|----------------------------------------|--------------------|
| Wayland (wlroots)         | :heavy_check_mark:       |                          |                          |                          |                                        |                    |
| Wayland (KDE)             |                          | :heavy_check_mark:       | :heavy_check_mark:       |                          |                                        |                    |
| Wayland (Gnome)           |                          | :heavy_check_mark:       | :heavy_check_mark:       |                          |                                        |                    |
| Windows                   |                          |                          |                          | :heavy_check_mark:       |                                        |                    |
| MacOS                     |                          |                          |                          |                          |   :heavy_check_mark:                   |                    |
| X11                       |                          |                          |                          |                          |                                        | :heavy_check_mark: |

- `wlroots`: This backend makes use of the [wlr-virtual-pointer-unstable-v1](https://wayland.app/protocols/wlr-virtual-pointer-unstable-v1) and [virtual-keyboard-unstable-v1](https://wayland.app/protocols/virtual-keyboard-unstable-v1) protocols and is supported by most wlroots based compositors.
- `libei`: This backend uses [libei](https://gitlab.freedesktop.org/libinput/libei) and is supported by GNOME >= 45 or KDE Plasma >= 6.1.
- `xdp`: This backend uses the [freedesktop remote-desktop-portal](https://flatpak.github.io/xdg-desktop-portal/#gdbus-org.freedesktop.portal.RemoteDesktop) and is supported on GNOME and Plasma.
- `x11`: Backend for X11 sessions.
- `windows`: Backend for Windows.
- `macos`: Backend for MacOS.



### Input Capture Support

| Desktop / Backend         | layer-shell              | libei                    | windows                  |   macos                                | x11 |
|---------------------------|--------------------------|--------------------------|--------------------------|----------------------------------------|-----|
| Wayland (wlroots)         | :heavy_check_mark:       |                          |                          |                                        |     |
| Wayland (KDE)             | :heavy_check_mark:       | :heavy_check_mark:       |                          |                                        |     |
| Wayland (Gnome)           |                          | :heavy_check_mark:       |                          |                                        |     |
| Windows                   |                          |                          | :heavy_check_mark:       |                                        |     |
| MacOS                     |                          |                          |                          |   :heavy_check_mark:                   |     |
| X11                       |                          |                          |                          |                                        | :heavy_check_mark: |

- `layer-shell`: This backend creates a single pixel wide window on the edges of Displays to capture the cursor using the [layer-shell protocol](https://wayland.app/protocols/wlr-layer-shell-unstable-v1).
- `libei`: This backend uses [libei](https://gitlab.freedesktop.org/libinput/libei) and is supported by GNOME >= 45 or KDE Plasma >= 6.1.
- `windows`: Backend for input capture on Windows.
- `macos`: Backend for input capture on MacOS.
- `x11`: Backend for X11 sessions (uses pointer barriers and the XTest extension).
