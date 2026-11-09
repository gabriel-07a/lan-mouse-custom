# Fresh install, reset and uninstall: where Lan Mouse keeps state

Status: implemented in this change set. The goal is the user requirement

> "quando o cara instalar a release não é para aparecer nenhum pc que já estava
> configurado, não quero que mantenha dados de configurações antigas."

A release installed over older builds must start without the old devices,
certificates, permissions, pending requests, address rules, portal consent
tokens or audit history. At the same time a normal package **upgrade** must not
silently delete a real user's configuration.

Design decision (after QA review): **cleaning is a reversible move, not a
delete.** The app-owned directories are renamed to a sibling
`lan-mouse.bak-<timestamp>` (atomic same-directory rename). The next start
therefore sees a pristine configuration, while the old devices/certificate/
audit log remain recoverable by renaming the backup back. Permanent deletion is
only done with an explicit `--purge` / `/PURGEUSERDATA`.

## 1. Inventory: every persistent path

All per-user state uses one directory name, `lan-mouse`. The app never writes
configuration anywhere else (custom `--config` / `--cert-path` are the only
exceptions and are deliberately not touched by reset/installer cleanup).

| OS | Path | Contents | Source |
| --- | --- | --- | --- |
| Linux | `${XDG_CONFIG_HOME:-~/.config}/lan-mouse/` | `config.toml` (clients, devices, incoming/outgoing policies, `denied_fingerprints`, address rules, trusted IPs), `config.toml.corrupt-<unix timestamp>` (only if the file could not be parsed; see below), `lan-mouse.pem` (certificate + private key) | `src/config.rs` (`default_path`, `CERT_FILE_NAME`, `backup_corrupt_config`) |
| Linux | `${XDG_STATE_HOME:-~/.local/state}/lan-mouse/` | `audit.jsonl` (+ `.1`), `input-capture.token`, `remote-desktop.token`, `clipboard.token` (portal consent tokens, `0600`) | `src/audit.rs`, `src/doctor.rs`, `input-capture/src/libei.rs`, `input-emulation/src/libei.rs`, `input-clipboard/src/portal.rs` |
| Linux | `${XDG_CACHE_HOME:-~/.cache}/lan-mouse/` | portal tokens written by older builds (migrated on next read, then removed) | legacy `token_dir` in the portal backends |
| Linux | `~/Desktop/de.feschber.LanMouse.desktop` | desktop shortcut created by the `.deb` postinst | `scripts/package-deb.sh` |
| Windows | `%LOCALAPPDATA%\lan-mouse\` | `config.toml`, `config.toml.corrupt-<unix timestamp>` (only if the file could not be parsed; see below), `lan-mouse.pem`, `audit.jsonl` (+ `.1`) | `src/config.rs` (`default_path`, `backup_corrupt_config`), `src/audit.rs` |
| Windows | `%APPDATA%\lan-mouse\` | only from portable/older builds; current builds never write it, cleaned defensively | installer/uninstaller and `--reset-config` |
| Windows | `%PROGRAMFILES64%\Lan Mouse\`, shortcuts, `HKLM\Software\de.feschber.LanMouse.Custom`, uninstall entry, firewall rule `Lan Mouse` | installation itself | `installer/windows/lan-mouse-custom.nsi` |
| cross-OS | in-memory | pending approval requests, pairing window | not persisted; disappear on restart |

No SQLite database, keychain entry or registry value is used for devices or
permissions: the `[[devices]]`, `[[address_rules]]`, `[authorized_fingerprints]`
and `[[denied]]` state all live inside `config.toml`.

There is **no `config.toml.bak` file**: a normal save replaces `config.toml` in
place. If `config.toml` cannot be parsed at startup, `src/config.rs` copies the
exact original bytes to a sibling `config.toml.corrupt-<unix timestamp>` (numeric
suffix on collision) and keeps the original file untouched, so the user can
repair it; the service then starts with defaults. Reset/installer cleanup moves
the whole `lan-mouse` directory, so a corrupt-config backup travels with it.
A corrupt config is never written back over the original.

## 2. Semantics: fresh install vs upgrade vs uninstall

| Operation | Windows | Linux `.deb` | Linux tarball / manual |
| --- | --- | --- | --- | --- |
| First install on a clean machine | nothing to move | nothing to move | nothing to move | nothing to move |
| Install over leftovers of older/test builds | interactive: prompt "start fresh? old configuration is MOVED to a backup" **default Yes**; silent (`/S`): move to backup by default | user data untouched (upgrade-safe); `apt purge` does not touch home data; reset explicitly | reset explicitly | reset explicitly |
| Upgrade of a real installation | same prompt; **No** keeps the configuration in place (or pass `/KEEPUSERDATA`) | user data untouched by design | user data untouched | user data untouched |
| Uninstall | GUI + daemon asked to shut down gracefully (`lan-mouse cli shutdown`) and force-killed with `taskkill /T /F` if that fails, per-user data **moved to a backup** (interactive asks, default Yes; silent moves too; `/KEEPUSERDATA` keeps in place, `/PURGEUSERDATA` deletes), old install dir wiped, shortcuts/firewall/registry removed | package files removed; **per-user data kept** (maintainer scripts never touch home dirs); `postrm purge` prints how to reset | move backups explicitly (`--reset-config` / helper script) | same |
| Restore after a clean install | rename `%LOCALAPPDATA%\lan-mouse.bak-<time>` back to `lan-mouse` | `mv ~/.config/lan-mouse.bak-<ts> ~/.config/lan-mouse` (same for state/cache) | same | same |
| Permanent cleanup | delete `lan-mouse.bak-*` manually, or use `/PURGEUSERDATA` | `--purge`, or delete the `.bak-*` directories | `--purge` / helper `--purge` | same |

Rationale: a *package upgrade* must never destroy a working configuration, but
the user asked for a **release install** to be clean. The Windows installer can
ask/act per install; Debian packages cannot (dpkg runs as root and has no idea
which of the machine's users ran the app), so Linux gets an explicit,
documented reset. Backing up instead of deleting keeps the clean install
reversible even when the prompt defaults to "clean".

## 3. Implemented mechanisms

### 3.1 `lan-mouse --reset-config [--dry-run] [--purge]` (cross-platform)

New module `src/reset.rs`, wired in `src/config.rs` before `Config::new` creates
any directory, so the command never re-creates the config it just moved away.

* checks the service is not running (`lan_mouse_ipc::is_service_running`) and
  refuses with a clear message instead of racing/moving files in use;
* resolves the config directory through `src/config.rs::default_path` (the same
  function the app uses) plus the state/cache/legacy locations;
* default: rename every existing app directory to a sibling
  `<path>.bak-<unix timestamp>` (one timestamp per run, numeric suffix on
  collision) — atomic, same filesystem, reversible;
* `--purge`: permanently delete instead of moving;
* `--dry-run`: lists the exact source and backup destinations, does nothing;
  `--dry-run`/`--purge` without `--reset-config` are rejected by clap;
* prints `moved`, `would move`, `permanently deleted`, `not present`, and a
  `restore with: mv ...` hint for every moved path;
* exits 0 on success (including "nothing to do"), 1 on I/O failure or while
  Lan Mouse is running.

Examples:

```sh
lan-mouse --reset-config --dry-run   # preview the paths and backup names
lan-mouse --reset-config             # move devices/certs/audit/tokens to .bak
lan-mouse --reset-config --purge     # delete them permanently
```

Limitations: a custom `--config` / `--cert-path` outside the app directories is
not touched; on Windows the installed binaries, shortcuts, firewall rule and
registry are removed by the uninstaller, not by this flag. OS-level permission
grants (portal consent, Windows firewall) are managed by the OS and are not
revoked by a reset; backups accumulate until deleted manually.

### 3.2 Windows installer / uninstaller (`installer/windows/lan-mouse-custom.nsi`)

* install: the running GUI and the daemon it spawned are first asked to shut
  down gracefully (`lan-mouse.exe cli shutdown` over the local IPC; the daemon
  then releases pressed keys/the pointer and removes its socket), then
  force-killed with `taskkill /F /T /IM lan-mouse.exe` filtered to the
  installing user if the graceful stop did not complete within ~3 s; then, if
  `%LOCALAPPDATA%\lan-mouse` or `%APPDATA%\lan-mouse` has content, **rename** it
  to `lan-mouse.bak-<yyyyMMdd-HHmmss>` before extracting the new binaries;
  * interactive prompt, default **Yes** ("Yes: move it to the backup and start
    fresh (recommended) / No: keep it in place (upgrade)");
  * silent: clean by default; `/KEEPUSERDATA` keeps the data in place,
    `/CLEANUSERDATA` forces the clean install, `/PURGEUSERDATA` deletes the old
    data permanently instead of backing it up;
  * previous `$INSTDIR` is wiped only when the registry says it is our own
    previous install directory (no stale binaries from older builds);
  * duplicate firewall rules are deleted before the new rule is added.
* uninstall: stops GUI + daemon first, moves the same two user-data directories
  to timestamped backups (interactive asks, default Yes; silent moves too;
  `/KEEPUSERDATA` keeps in place, `/PURGEUSERDATA` deletes), then removes
  shortcuts, firewall rule, registry keys and `$INSTDIR`.
* guarantees: only paths below `%LOCALAPPDATA%\lan-mouse`,
  `%APPDATA%\lan-mouse`, their `.bak-*` siblings, `$INSTDIR`, the Start
  Menu/Desktop/Startup shortcuts, the two registry keys and the diagnostics log
  `%TEMP%\lan-mouse-install.log` are touched.

### 3.3 Linux helper (`scripts/install-clean.sh`)

For installs that are not the Windows installer, and for users who want a
scripted reset without an up-to-date binary. Default: move each app directory
to `<path>.bak-<unix timestamp>` (`mv`, atomic within the same parent);
`--purge` deletes permanently; `--dry-run` lists only; `--yes` skips the
prompt; `--no-kill` skips stopping a running `lan-mouse` (stopped by default
so files are not in use). Validates every path (absolute, ends in
`/lan-mouse`, not below `/`), prints the list and asks before acting.

### 3.4 Debian package (`scripts/package-deb.sh`)

`prerm` keeps stopping the daemon and removing the desktop shortcuts on
remove/purge. A new `postrm` does **not** touch user data; on `purge` it prints
the exact per-user paths and how to reset them (`lan-mouse --reset-config`
moves them to a reversible backup, `--purge` deletes; helper script
`scripts/install-clean.sh`), and refreshes desktop/icon caches after removal.

## 4. Tests

* Unit tests in `src/reset.rs`: path ownership (`is_owned_app_dir` rejects
  `/lan-mouse`, relative paths and unrelated dirs), unique/reversible backup
  names, move preserves content and can be restored by renaming back, dry-run
  does not move, `--purge` deletes without a backup, symlink handling,
  XDG/HOME and Windows AppData path resolution.
* `tests/packaging.rs` (integration): runs the real binary with temporary
  `HOME`/`XDG_*`/`AppData` values (never the host configuration), asserts that
  `--reset-config` moves only `lan-mouse` directories to `.bak-*` backups,
  keeps unrelated files, leaves everything in place with `--dry-run`, deletes
  only with `--purge`, and refuses while a fake service socket is listening;
  plus string guards for the NSIS backup/clean-install/uninstall paths, the
  `.deb` postrm and the helper script.
* `.github/workflows/release.yml` Windows smoke test: seeds a stale
  `%LOCALAPPDATA%\lan-mouse\config.toml` + `lan-mouse.pem` and
  `%APPDATA%\lan-mouse\config.toml` **before** running the silent installer,
  then asserts that the original paths are gone (fresh install), that a
  `lan-mouse.bak-*` backup now contains the stale data (reversible), and that
  the silent uninstaller also moves the per-user directories created by the
  `doctor` run. An unrelated file in `%LOCALAPPDATA%` must survive.
* Manual matrix (Windows 10/11 + Ubuntu 26):

| Step | Expected |
| --- | --- |
| install old build, configure a peer, install release over it | installer prompt; after Yes no devices/certs/permissions; data present under `%LOCALAPPDATA%\lan-mouse.bak-<time>` and restorable |
| same, answering No (or `/S /KEEPUSERDATA`) | previous devices still listed (upgrade path) |
| silent install `/S` over an old build | moves to backup by default; `/S /KEEPUSERDATA` keeps; `/S /CLEANUSERDATA` forces the move; `/S /PURGEUSERDATA` deletes |
| uninstall (interactive Yes / silent) | config moved to `.bak-*`, no `lan-mouse.exe` running, firewall rule and shortcuts gone |
| Ubuntu: `apt install` upgrade over old version | devices/permissions/audit preserved (intentional) |
| Ubuntu: `lan-mouse --reset-config --dry-run` | lists `~/.config/lan-mouse`, `~/.local/state/lan-mouse`, `~/.cache/lan-mouse` and their backup names, touches nothing |
| Ubuntu: `lan-mouse --reset-config` while daemon runs | refuses with a clear message; run again after quitting → moved to `.bak-*`, next start fresh |
| Ubuntu: `lan-mouse --reset-config --purge` | removes the directories permanently, no `.bak-*` left |

## 5. Risks and open points

* **Backups accumulate.** Every reset/reinstall keeps a `lan-mouse.bak-*`
  directory per app directory until the user deletes it. This is the intended
  trade-off (reversibility); README/DOC document where they are and how to
  remove them. `--purge` skips them entirely.
* **Multi-user machines / elevated installer.** The NSIS installer is
  `RequestExecutionLevel admin`; `$LOCALAPPDATA`/`$APPDATA` refer to the user
  who approved the UAC prompt. If a different admin account is used, another
  user's data is not cleaned; uninstalling from each account (or
  `lan-mouse --reset-config` executed as that user) is required. Same for
  Debian: root cannot know/enumerate users, hence no automatic home cleanup.
* **Prompt fatigue.** The install prompt defaults to cleaning (move to
  backup), which is what the requirement asks for and is explicitly confirmed
  by the user; users upgrading with a real configuration should answer "No"
  (documented in README) or use `/KEEPUSERDATA`.
* **Custom paths.** `--config`, `--cert-path` and `LAN_MOUSE_AUDIT_LOG` can
  point outside the app directories; those files are intentionally not touched
  by reset/installer cleanup (touching arbitrary user paths is riskier than
  leaving them).
* **Windows silent upgrades.** `/S` backs up by default; automation that wants
  to preserve a configuration must pass `/KEEPUSERDATA`. This is the trade-off
  requested by the user, documented in README.
* **Portal tokens.** Moving them means GNOME/KDE will ask for clipboard /
  input capture / remote desktop consent again on the next start. That is the
  intended consequence of a fresh install; the backups still contain the old
  tokens (bearer capabilities) until the user deletes the backup directories.
  Portal prompts reappear, but an already granted permission may remain valid
  in the OS settings until revoked there (a reset cannot revoke it).
* **Antivirus / locked files.** If the daemon is still running, Windows may keep
  the `.pem`/`config.toml` locked and the rename fails; both the installer and
  `--reset-config` stop/refuse first, and the installer aborts with a clear
  message instead of continuing with the old configuration in place. Other
  processes reading the files are out of scope.
* **Pre-2023 Windows builds.** Builds older than the `XDG_CONFIG_HOME` change
  (`e6677c3`) stored `config.toml` in the working directory; those files cannot
  be discovered automatically. The installer wipes `$INSTDIR`, which covers the
  common portable layout, but a config left in an arbitrary folder must be
  removed manually.
* **Backup contents are sensitive.** A backup contains the certificate/private
  key and, on Linux, portal consent tokens (bearer capabilities); delete old
  `lan-mouse.bak-*` directories once you are sure you do not need them.
