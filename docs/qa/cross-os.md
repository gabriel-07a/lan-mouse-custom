# Cross-OS / build-configuration QA ledger (task-7)

Scope: cross-OS compile matrix, feature combinations, build.rs/cfg discipline,
packaging/installers/scripts/CI, docs truth, Nix packaging.
Owner: crossos-auditor. Baseline: HEAD 6708b79 + working tree (repo builds and
tests were green at task start; this file records what changed since).

**Wire/IPC compatibility: UNCHANGED.** No file touched by this task changes
lan-mouse-proto byte layouts or lan-mouse-ipc request/response shapes. The only
dependency-graph change (shadow-rs default features) is build-time only and does
not change the reported version/commit strings.

## How the matrix was run

Host: Linux x86_64, rustc 1.99.0, 20 cores; installed targets
x86_64-unknown-linux-gnu, x86_64-pc-windows-gnu, x86_64-pc-windows-msvc,
x86_64-apple-darwin.

* Linux: cargo check against the normal target dir.
* Cross targets: no C cross toolchain is installed (no mingw gcc, no clang, no
  zig). cargo check never links, so the C build scripts (ring, libgit2-sys, ...)
  were satisfied with wrapper scripts in target/qa-matrix/toolchain/ that create
  the .o/.a/.lib artefacts those build scripts expect (target-specific CC_/AR_
  env vars only; host build-dependencies keep the real host toolchain).
  **Limitation: this type-checks the Rust crates for the target. It is not a
  link test and does not compile the C parts**; those are covered by native CI
  runners (rust.yml Windows jobs and release.yml).
* Feature matrix driver: target/qa-matrix/matrix.sh; results and logs under
  target/qa-matrix/{results,logs}/ (gitignored, not shipped).
* Result captured at tree state described below; a second full run at freeze is
  recorded at the end of this file.

## Matrix results

### Linux host (all PASS)

workspace-default, workspace-nodefault, workspace-allfeatures, root-default
(gpui + all backends), root-nodefault, root-nodefault-allbackends,
root-gpui-only, root-gpui-allbackends, root-gtk-only, root-gtk-allbackends,
root-allfeatures, and each backend feature alone: layer_shell_capture,
x11_capture, libei_capture, wlroots_emulation, libei_emulation, rdp_emulation,
x11_emulation; member crates input-capture / input-emulation / input-event with
--no-default-features. 23 combos (target/qa-matrix/results/host.txt).

### x86_64-pc-windows-gnu (type-check only)

| Combo | Result |
| --- | --- |
| root default (gpui + all backends) | PASS |
| root --no-default-features | PASS |
| root --no-default-features --features gpui | PASS |
| root --no-default-features --features gpui + all backends | PASS |
| root --no-default-features --features all backends | PASS |
| workspace --exclude lan-mouse-gtk --all-targets | PASS |
| every member crate, default and --no-default-features | PASS |
| root --features gtk | FAIL (expected: GTK4 C deps have no cross build) |
| root --all-features | FAIL (expected: includes gtk) |
| workspace --all-targets (full) | FAIL (expected: includes lan-mouse-gtk member) |

### x86_64-apple-darwin (type-check only; macOS is dormant)

Same PASS/FAIL set as windows-gnu: all shipped root feature combinations and all
member crates PASS; only the GTK4-containing combos fail (expected; GTK frontend
is Linux-only). This confirms the dormant cfg(target_os = "macos") modules still
type-check, but they are not built, tested or shipped.

### x86_64-pc-windows-msvc

Not validated on this host: the MSVC target additionally needs MSVC-style
lib.exe semantics (ring/zlib archives) which the fake toolchain cannot emulate.
Member crates that need no C toolchain type-check. Native CI (windows-2022 /
windows-2025, and the GPUI MSVC release job) is the real verification.

### GPUI standalone workspace (lan-mouse-gpui)

cargo check --all-targets, --no-default-features and --locked all PASS. A local
debug link fails only because the host lacks libxkbcommon-x11 (build environment,
not code); release builds happen in CI/nix.

### Expected-unsupported combos (documented, not bugs)

* --features gtk / --all-features on non-Linux: the legacy GTK4 frontend is
  Linux-only; root default features do not select it.
* cargo check --workspace on non-Linux: lan-mouse-gtk is still an unconditional
  workspace member (its dependency is optional only for the root crate). CI
  Windows jobs now use --exclude lan-mouse-gtk; the release workflow builds the
  root package only (default features, no gtk), so what users ship is exercised.
* C/native code (ring, libgit2-sys, MSYS2 GTK stack) is not compiled locally for
  cross targets by design of this harness.

## Findings (found / fixed in this task)

Severity: P1 crash/hang/data-loss/security/connectivity; P2 incorrect observable
behaviour or cross-OS build break; P3 robustness/telemetry/docs.

| ID | Sev | Location | Symptom | Root cause | Fix | Verification | Status |
| --- | --- | --- | --- | --- | --- | --- | --- |
| XOS-01 | P1 | .github/workflows/release.yml | Windows smoke test failed intermittently with only "Process completed with exit code 1"; no artefacts to diagnose | one giant step, some exit codes unchecked, GUI-subsystem stdout piped, nothing uploaded on failure | split into install / doctor / uninstall steps; check process exit codes; redirect stdout/stderr to files; if: always() diagnostics upload (installer log, doctor json/err, firewall rule, signatures, registry) | YAML validated; workflow structure reviewed; real Windows run still required | FIXED (needs CI run) |
| XOS-02 | P1 | installer/windows/lan-mouse-custom.nsi | Launch Lan Mouse on the finish page was checked by default and ran GUI+daemon elevated | MUI default for MUI_FINISHPAGE_RUN is checked | added MUI_FINISHPAGE_RUN_NOTCHECKED (option stays available, opt-in) | static review; NSIS not compilable on Linux host | FIXED |
| XOS-03 | P1 | firewall/lan-mouse.xml | firewalld users block discovery: service definition only opened UDP 4242 | discovery/beacons use UDP 4245 (DEFAULT_DISCOVERY_PORT) and need inbound too | added 4245/udp; documented both ports and configurability | XML parsed; matches code defaults | FIXED |
| XOS-04 | P1 | installer/windows/lan-mouse-custom.nsi | netsh failures were ignored: install "succeeds" with no inbound firewall rule | nsExec::ExecToLog exit codes never checked | ExecToStack + exit-code capture, %TEMP% lan-mouse-install.log, interactive warning; non-fatal by design (policy-managed hosts) | static review | FIXED |
| XOS-05 | P1 | installer/windows/lan-mouse-custom.nsi | upgrade/uninstall taskkill /F /T killed every user's daemons and could leave keys stuck on a peer | no graceful stop; no per-user filtering | graceful lan-mouse.exe cli shutdown (new IPC request) + ~3 s wait loop, then filtered taskkill, each step logged; other users' daemons are spared | FrontendRequest::Shutdown + cli shutdown verified present; NSIS static review | FIXED |
| XOS-06 | P1 | .github/workflows/release.yml | release artefacts had no signing/checksums (SmartScreen cannot be cleared, tampering undetectable) | nothing published | optional secrets-gated Authenticode signing of exes+installer, SHA-256 SHA256SUMS.txt published with every release; unsigned builds skip signing cleanly | YAML validated; signing steps are inert without secrets | FIXED (needs secret to sign) |
| XOS-07 | P2 | Cargo.toml (root) | shadow-rs default features pulled git2/libgit2-sys+tzdb into the target dependency graph of the shipped binary; cross/MSVC builds needed a libgit2 C build for nothing | shadow-rs documents default-features = false for the runtime dependency (only the build-dependency needs git metadata) | shadow-rs default-features = false, features = ["metadata"] | host cargo build --workspace --all-targets + cargo test --workspace green; cargo tree -i git2 --target windows-gnu shows build-dependencies only; Cargo.lock unchanged; cross matrix went from blocked to green | FIXED |
| XOS-08 | P2 | .github/workflows/rust.yml | Windows check/clippy/test jobs ran --workspace including the Linux-only GTK4 member, a deterministic failure when dispatched | workspace-wide flags on Windows | --exclude lan-mouse-gtk on Windows check/test/clippy; root default features never enable gtk | YAML validated; mirrors the supported release build | FIXED |
| XOS-09 | P2 | scripts/package-deb.sh | GPUI frontend needs libvulkan1 at runtime but the .deb only depended on X11/Wayland/xkb libs | hand-written Depends missed the Vulkan loader (blade-graphics loads it) | added libvulkan1 to Depends, mesa-vulkan-drivers to Recommends | real dpkg-deb build inspected with dpkg-deb -f | FIXED |
| XOS-10 | P2 | scripts/package-deb.sh (prerm) | desktop shortcut never removed on uninstall | find /home -maxdepth 2 cannot reach /home/<user>/Desktop/<file> (depth 3) | -maxdepth 4 -type f with the app-specific file name only, plus /root/Desktop | bash -n; logic verified against postinst path | FIXED |
| XOS-11 | P2 | scripts/package-deb.sh | .deb did not ship the systemd user unit or license text (README told users to copy manually) | packaging gap | installs /usr/lib/systemd/user/lan-mouse.service and /usr/share/doc/lan-mouse/copyright + NOTICE | real .deb build inspected | FIXED |
| XOS-12 | P2 | installer/windows/lan-mouse-custom.nsi | manual NSIS build without /DAPPVERSION produced a 0.2.4-labelled installer for a 0.2.5 tree | fallback define not bumped | fallback 0.2.5 + note to keep in sync | grep | FIXED |
| XOS-13 | P2 | service/lan-mouse.service, nix/hm-module.nix | unit relied on the implicit SIGTERM default, no stop timeout/backoff/hardening; stop semantics undocumented | minimal upstream unit | explicit KillSignal=SIGTERM, TimeoutStopSec=5, RestartSec=1, NoNewPrivileges=yes, comment why no PrivateTmp/ProtectSystem (X11 socket in /tmp/.X11-unix); daemon now handles SIGTERM (session-auditor) | systemd-analyze verify: only expected missing-binary/target warnings on this headless host | FIXED |
| XOS-14 | P2 | nix/hm-module.nix, default.nix, nix/default.nix | HM module put mkIf elements inside a list of string; legacy nix-build shipped only the service binary (runtime "GPUI frontend binary not found"); platforms.all on a Linux-only package | invalid module composition + stale packaging | lib.optional list composition, After + stop/restart options; legacy default.nix symlinkJoin of main+gpui; platforms.linux | Nix not installed on host; reviewed against Nix module conventions (needs a Nix evaluation) | FIXED (needs nix eval) |
| XOS-15 | P2 | de.feschber.LanMouse.desktop | StartupNotify=false makes StartupWMClass ineffective (no launch feedback / window association) | desktop spec: WMClass only used when startup notification is enabled | StartupNotify=true | desktop-file-validate passes | FIXED |
| XOS-16 | P2 | Cargo.toml (root) | stale [package.metadata.bundle] referenced non-existent target/icon.icns and build-aux/macos-lsui-element.plist; cargo-bundle is not used, macOS is dormant | dead config from upstream | removed [package.metadata.bundle] and .osx sections | grep: no remaining references; cargo metadata unaffected | FIXED |
| XOS-17 | P2 | README.md, DOC.md | docs contradicted the code: X11 capture advertised (stub: NotImplemented); "wlroots/X11 clipboard not implemented" (implemented); wlroots modifier caveat (backend derives mods from key events); tokens said ~/.cache (now XDG_STATE_HOME with cache migration); manual build omitted lan-mouse-gpui (app exits 1 without it); release link pointed at upstream; lan-mouse-cli binary name; firewall section only 4242; nix-build output; systemd note | docs drifted from implementation | fixed all listed claims; kept documented limitations (X11 INCR, UIPI, GNOME v1 capture prompt) | tests/packaging.rs string guards preserved; re-read after patch | FIXED |
| XOS-18 | P3 | scripts/install-clean.sh | dead empty case blocks, truncated comment, GPUI frontend process not stopped | merge leftovers | removed dead blocks, fixed comment, stops lan-mouse and lan-mouse-gpui, added lan_mouse_running() helper | bash -n + sandboxed --dry-run and move run in a temp HOME (backups created, unrelated files kept, config moved) | FIXED |
| XOS-19 | P3 | installer/windows/lan-mouse-custom.nsi header | comment said the payload contains "GTK dlls" | stale since the GPUI split | corrected to lan-mouse.exe + lan-mouse-gpui.exe + MinGW DLLs | review | FIXED |
| XOS-20 | P3 | .github/workflows/release.yml | release builds were not --locked (lockfile could drift mid-release) | habit | --locked on Linux/Windows/gpui release builds | YAML validated | FIXED |
| XOS-21 | P3 | .github/workflows/rust.yml | workflows were workflow_dispatch only, so the README CI badge never reflects pushes | manual-only CI | added push branches main + pull_request (expensive release/frontend workflows stay manual/tag-gated, per Lead decision) | YAML validated | FIXED |
| XOS-22 | P3 | build-aux/de.feschber.LanMouse.yml | Flatpak manifest is stale (debug build, service-only, no GPUI frontend, llvm19 path under llvm20 SDK) | prototype from before the frontend split | marked NOT MAINTAINED / NOT A RELEASE ARTIFACT in a header (not deleted, per Lead) | review | DOCUMENTED |
| XOS-23 | P3 | spike/gpui, .github/workflows/gpui-spike.yml | prototype workspace + manual workflow are release-irrelevant | spike leftovers | not touched (out of a bug-fix pass); **recommend removing before a public release** | - | DEFERRED |

## Verified-at-HEAD items from the Lead backlog (not re-reported as bugs)

* Firewall delete-before-add and profile=private,domain,public were already fixed
  at HEAD (nsi); the remaining gap (ignored netsh exit codes) is XOS-04.
* Version consistency: Cargo.toml / lan-mouse-gpui = 0.2.5; NSIS fallback fixed
  (XOS-12); flake/nix derive the version from Cargo.toml; no CHANGELOG exists;
  dist/ holds ignored local 0.2.0 artefacts only.
* Ports: transport 4242/udp (config port), discovery 4245/udp (config
  discovery_port); IPv4-only is the product decision (README/DOC agree).
* Shipped matrix: Linux (.deb/tar/Nix) and Windows (zip/NSIS); macOS is dormant and
  is not built or shipped by release.yml; README/DOC now state this consistently.
* cargo build (default features) needs the separately built lan-mouse-gpui binary
  next to it; README build/manual-install sections now say so. lan-mouse daemon
  does not spawn a frontend, so the systemd unit only needs the service binary
  (the deb ships both anyway).
* tests/packaging.rs and tests/qa_installer_clean.rs are read-only for this task;
  their assertions were kept satisfied (verified after the freeze re-run below).

## Cross-scope findings reported to Lead (not fixable in this task's files)

1. AGENTS.md says "connection requests are TCP on the same port", contradicting
   DOC.md ("does not use the old TCP request channel") and the code (DTLS over
   UDP). Documentation of the agent instructions should be corrected by its owner.
2. tests/packaging.rs only does string checks; NSIS semantic changes cannot be
   regression-tested locally (no makensis on this host). CI compiles NSIS.
3. rdp emulation backend still does not persist restore_token
   (docs/audit-ubuntu-permissions.md section 3 item 11) - open follow-up.
4. A local debug/host build of the GPUI frontend needs libxkbcommon-x11
   development files; README's Ubuntu dependency list has libxkbcommon-x11-dev
   but the error is non-obvious. Documented requirement unchanged.

## Verification commands and results

* Linux host feature matrix: 23/23 PASS (target/qa-matrix/results/host.txt).
* CARGO_TARGET_DIR=target/xcheck-gnu and xcheck-darwin: shipped root combos +
  member crates PASS; only GTK-containing combos fail (expected).
* cargo build --workspace --all-targets + cargo test --workspace: green before
  this task's freeze (target/qa-matrix/logs/host-workspace-build.log,
  host-workspace-test.log); re-run at freeze below.
* bash -n scripts/install-clean.sh scripts/package-deb.sh: OK.
* Sandboxed scripts/install-clean.sh --dry-run / --yes with a temp HOME: backup
  created, unrelated files preserved, no deletion.
* Real scripts/package-deb.sh 0.2.5-test <fake-target-dir>: valid .deb
  (dpkg-deb -c shows both binaries, desktop entry, icons, systemd user unit,
  copyright/NOTICE; dpkg-deb -f shows the new Depends/Recommends/Architecture).
* python3 yaml.safe_load on every touched workflow: OK.
* desktop-file-validate de.feschber.LanMouse.desktop: OK.
* systemd-analyze verify service/lan-mouse.service: no syntax/semantic errors
  (only expected missing-binary/target warnings on this headless host).
* git status --porcelain Cargo.lock: clean (Cargo.lock untouched).

## Residual risks / needs real hardware or CI

* Authenticode signing steps compile by construction but were never executed (no
  certificate secrets here); the release stays unsigned until the two secrets
  WINDOWS_CODESIGN_PFX_BASE64 / WINDOWS_CODESIGN_PFX_PASSWORD are configured.
* NSIS script cannot be compiled on this Linux host (no makensis). The release
  workflow (windows-latest, choco nsis) must compile it; the new macros use only
  standard NSIS/nsExec constructs, but the first CI run is the real gate.
* Windows silent install/doctor/uninstall smoke test was restructured but not run.
* Windows MSVC cross type-check is unverified locally; native Windows CI covers it.
* macOS stays dormant: it type-checks for the target but is neither built nor
  tested; the README/DOC statement is intentional and must not regress into a
  support claim.
* Real-hardware checks still needed for the documented limitations (X11 INCR
  clipboard, UIPI/elevated windows, GNOME v1 capture prompt, mixed-DPI
  multi-monitor, wlroots modifier behaviour on Sway/Hyprland/Wayfire).
* The matrix was captured while other teammates edited the tree; transient
  compile errors seen mid-run were in-flight edits, not project bugs. A clean
  matrix run is needed at freeze (below).

## Freeze re-run

Frozen tree: HEAD 6708b79 + 72 modified files (whole-team QA working tree),
2026-11-09T02:15Z (see target/qa-matrix/freeze-state.txt). Note that teammates
were still editing during the first pass; two transient compile states were
observed and are recorded below, followed by the clean re-runs.

Authoritative gates:

* cargo build --workspace --all-targets => BUILD_EXIT=0
  (target/qa-matrix/logs/freeze-build.log).
* cargo test --workspace => TEST_EXIT=0, 24/24 test binaries green
  (target/qa-matrix/logs/freeze-test2.log). An earlier run (freeze-test.log)
  caught one in-progress test in input-capture
  (display_util::absolute_sub_pixel_moves_keep_their_direction); its owner
  landed the fix and the final run above is green.

Cross-OS type-check matrix at freeze:

* Linux host feature matrix: 23/23 PASS
  (target/qa-matrix/results/host-freeze.txt), including workspace
  default/no-default/all-features, gpui/gtk frontends, every backend feature
  alone and both member-crate feature sets.
* x86_64-pc-windows-gnu: root default, --no-default-features, gpui-only,
  gpui+all-backends and all-backends-only PASS; workspace --exclude
  lan-mouse-gtk PASS; every member crate PASS. Only the expected GTK4 C-deps
  combos fail (root --features gtk, --all-features, full workspace)
  (target/qa-matrix/results/windows-gnu-freeze2.txt). An earlier freeze run had
  failed on a mid-edit input-capture/src/windows/event_thread.rs BOOL import;
  the owner fixed it (windows::core::BOOL) and the re-run above is green.
* x86_64-apple-darwin: same shipped combos PASS
  (target/qa-matrix/results/apple-darwin-freeze.txt); GTK combos fail as
  expected; macOS remains dormant/unshipped.
* x86_64-pc-windows-msvc: not locally verifiable with the fake toolchain;
  workspace --exclude lan-mouse-gtk and non-C member crates PASS
  (target/qa-matrix/results/windows-msvc-freeze.txt). Native Windows CI is the
  gate for this target.
* Cargo.lock untouched and clean at freeze (git status --porcelain Cargo.lock).

Caveat: the cross-target runs are cargo-check only (fake C toolchain, see the
method section); native CI runners are required for the C/link parts and for
the real Windows/macOS runtime behaviour.

## Post-freeze doc sync (Lead follow-up, task-7)

* docs/audit-fresh-install.md: removed the phantom config.toml.bak claim (no
  code ever created one) and documented the real corrupt-config backup
  (config.toml.corrupt-<unix timestamp> in src/config.rs, original kept);
  installer/uninstaller text now matches the graceful-stop + filtered-taskkill
  flow and the %TEMP%/lan-mouse-install.log diagnostic path; removed leftover
  dangling fragments from earlier merges (macOS TCC sentence, "Also",
  duplicated wording).
* AGENTS.md: transport corrected to DTLS-over-UDP 4242 + discovery UDP 4245
  (was: "connection requests are TCP"); capture backend order corrected to
  libei -> layer-shell -> X11 -> Windows -> macOS (no dummy fallback, returns
  CaptureCreationError::NoAvailableBackend); cfg example corrected to
  cfg(layer_shell_capture) with rustc-check-cfg note.
* tests/packaging.rs needles for docs/audit-fresh-install.md were re-checked and
  are all still present.

## Final doc-truth pass (Lead follow-ups, task-7)

* docs/audit-windows-permissions.md section 3.1: annotated the historical
  "Windows probes are infallible by construction" claim - the Windows capture
  probe is now fallible (WindowsInputCapture::new / EventThread report real
  setup errors through doctor), so probe output may legitimately differ;
  emulation probe remains Ok by construction. Audit text otherwise untouched.
* X11 capture re-verified across README/DOC/roadmap/matrix/feature list:
  supported list, caveat, roadmap entry, capture matrix, backend description
  and the conditional-compilation note all say X11 input capture is NOT
  implemented (receive-only); DOC.md has the same statement plus a note on the
  architecture diagram; Cargo.toml documents that the x11_capture feature is a
  stub (x11_emulation is the working X11 path).
* scripts/qa-two-instances.sh retargeted to the IPv4-only decision:
  duplicate scenario configures both sides over IPv4 loopback
  (127.0.0.1/127.0.0.2), asserts exactly 1 configured row per side, that the
  row is bidirectional (incoming session attached, no extra adopted row) and
  that no default-port 4242 dial exists; dual scenario uses two IPv4 loopback
  addresses; ipv6_only is HISTORICAL/SKIPPED (kept callable, always prints
  SKIP, removed from the default pass/fail run); IPv6 removed from the
  pre-trusted rules; discovery-4245 host-collision explanation kept; stale
  "expected to fail before fixes" text replaced. Validation command:
  QA_BASE_DIR=$(mktemp -d) QA_PORT_A=43931 QA_PORT_B=43932
  ./scripts/qa-two-instances.sh dual duplicate

### Frozen-tree matrix (hard freeze, Lead-owned tree)

Run at 2026-11-09T02:30Z on the frozen tree (HEAD 6708b79 + 77 dirty files, state
snapshot in target/qa-matrix/frozen-state.txt; Cargo.lock clean). Driver:
target/qa-matrix/matrix.sh, results in target/qa-matrix/results/*-frozen.txt.

**Linux feature matrix: 23/23 PASS** (target/qa-matrix/results/host-frozen.txt):
workspace-default, root-default (gpui + all backends), root-nodefault,
workspace-nodefault, root-gpui-only, root-gtk-only, root-gpui-allbackends,
root-gtk-allbackends, root-nodefault-allbackends, root-allfeatures,
workspace-allfeatures, each backend feature alone (layer_shell_capture,
x11_capture, libei_capture, wlroots_emulation, libei_emulation, rdp_emulation,
x11_emulation), member-nodefault-{input-capture,input-emulation,input-event},
member-default-{input-capture,input-emulation}.

**x86_64-pc-windows-gnu (cargo check, fake C toolchain): 15 PASS / 3 expected
FAIL** (target/qa-matrix/results/windows-gnu-frozen.txt). PASS: root-default,
root-nodefault, root-gpui-only, root-gpui-allbackends,
root-nodefault-allbackends, workspace-exclude-gtk, and all member crates
(member-nodefault-{input-capture,input-emulation,input-event,input-clipboard,
lan-mouse-ipc,lan-mouse-cli,lan-mouse-proto}, member-default-{input-capture,
input-emulation}). The 3 FAILs are root-gtk, root-allfeatures and
workspace-default, all with the same expected cause: GTK4 C dependencies
(glib-sys/gio-sys/gobject-sys/gdk4-sys/...) cannot be cross-built on this host
(Linux-only legacy frontend, not part of the shipped Windows build). No Rust
compile error in any crate.

**x86_64-apple-darwin (cargo check, fake C toolchain): 15 PASS / 3 expected
FAIL** (target/qa-matrix/results/apple-darwin-frozen.txt) - identical PASS set
and identical expected GTK4 C-dependency failures as windows-gnu; macOS remains
dormant/unshipped.

Methodology limitation unchanged: these cross-target runs type-check the Rust
crates only (fake CC/AR wrappers, no link, no C compilation); the native CI
runners remain authoritative for the C/link parts and runtime behaviour, and
windows-msvc cannot be locally verified with this harness.
* post-freeze delta re-checked (2026-11-09T02:36Z, HEAD 6708b79 + 77 dirty files incl. the authorized src/service.rs fixes and cosmetic string edits): host cargo check --workspace PASS; windows-gnu root-default PASS, root-nodefault PASS; apple-darwin root-default PASS, root-nodefault PASS (cargo check with fake C cross-toolchain; no link/C validation).
