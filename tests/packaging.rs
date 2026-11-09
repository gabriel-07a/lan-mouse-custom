//! Guards for the pieces that make the desktop integration work but cannot be
//! exercised by a normal unit test: the installer scripts, the packaged icon
//! and the desktop entry.
//!
//! These are cheap string checks, but they catch the regressions that hurt
//! most: a shortcut that loses its icon, a .deb that stops shipping the icon
//! theme files, or a package that forgets to remove the desktop shortcut.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    // Normalise line endings: the Windows CI checkout converts checked-in
    // files to CRLF (core.autocrlf), which used to break literal assertions
    // such as the Cargo.toml default-features block.
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn exists(relative: &str) -> bool {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(relative)
        .exists()
}

/// Per-user test sandbox: every environment variable that decides where Lan
/// Mouse stores data points into a temporary directory, so the tests below can
/// exercise the binary's real `--reset-config` path without ever touching the
/// configuration of the machine running the test suite.
struct FakeEnv {
    root: PathBuf,
}

static NEXT_FAKE_ENV: AtomicU64 = AtomicU64::new(0);

impl FakeEnv {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let unique = NEXT_FAKE_ENV.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!(
            "lm-packaging-{label}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("fake env root");
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// The binary under test with the fake environment applied.
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lan-mouse"));
        command
            .env("HOME", self.home())
            .env("USERPROFILE", self.home())
            .env("XDG_CONFIG_HOME", self.path("xdg-config"))
            .env("XDG_STATE_HOME", self.path("xdg-state"))
            .env("XDG_CACHE_HOME", self.path("xdg-cache"))
            .env("XDG_RUNTIME_DIR", self.path("runtime"))
            .env("LOCALAPPDATA", self.path("local-appdata"))
            .env("APPDATA", self.path("roaming-appdata"))
            .env_remove("LAN_MOUSE_AUDIT_LOG");
        command
    }

    /// Directories the current platform's build is expected to own.
    fn app_data_dirs(&self) -> Vec<PathBuf> {
        #[allow(unused_mut)]
        let mut dirs = Vec::new();
        #[cfg(unix)]
        {
            dirs.push(self.path("xdg-config").join("lan-mouse"));
            dirs.push(self.path("xdg-state").join("lan-mouse"));
            dirs.push(self.path("xdg-cache").join("lan-mouse"));
        }
        #[cfg(windows)]
        {
            dirs.push(self.path("local-appdata").join("lan-mouse"));
            dirs.push(self.path("roaming-appdata").join("lan-mouse"));
        }
        dirs
    }

    /// Create a stale device/config tree plus unrelated files that must survive.
    fn create_stale_data(&self) -> PathBuf {
        for dir in self.app_data_dirs() {
            fs::create_dir_all(&dir).expect("stale app dir");
            fs::write(dir.join("config.toml"), "# stale config from an old build")
                .expect("stale config");
            fs::write(dir.join("lan-mouse.pem"), "stale certificate").expect("stale cert");
        }
        let unrelated = self.path("keep-me");
        fs::create_dir_all(&unrelated).expect("unrelated dir");
        fs::write(unrelated.join("important.txt"), "do not touch").expect("unrelated file");
        unrelated
    }
}

impl Drop for FakeEnv {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run_reset(env: &FakeEnv, args: &[&str]) -> std::process::Output {
    env.command()
        .arg("--reset-config")
        .args(args)
        .output()
        .expect("run lan-mouse --reset-config")
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "--reset-config failed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Sibling `lan-mouse.bak-*` directories created next to `dir`.
fn backups_of(dir: &Path) -> Vec<PathBuf> {
    let Some(parent) = dir.parent() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("lan-mouse.bak-"))
        })
        .collect()
}

#[test]
fn windows_installer_creates_shortcuts_and_keeps_the_icon() {
    let script = read("installer/windows/lan-mouse-custom.nsi");

    assert!(script.contains("!include \"MUI2.nsh\""), "modern UI");
    assert!(script.contains("MUI_ICON"), "installer icon");
    assert!(
        script.contains("CreateShortCut \"$DESKTOP\\Lan Mouse.lnk\""),
        "desktop shortcut"
    );
    assert!(
        script.contains("CreateShortCut \"$SMPROGRAMS\\Lan Mouse\\Lan Mouse.lnk\""),
        "start menu shortcut"
    );
    assert!(script.contains("$INSTDIR\\icon.ico"), "shortcut icon");
    assert!(script.contains("taskkill"), "close the old instance");
    assert!(script.contains("WriteUninstaller"), "uninstaller");
    assert!(
        script.contains("MUI_FINISHPAGE_RUN"),
        "launch after install"
    );
    assert!(script.contains("netsh advfirewall"), "firewall rule");
    assert!(
        script.contains("CreateShortCut \"$SMSTARTUP\\Lan Mouse.lnk\""),
        "start with Windows shortcut"
    );
    assert!(
        script.contains("Delete \"$SMSTARTUP\\Lan Mouse.lnk\""),
        "the startup shortcut must be removed on uninstall"
    );
}

/// The GPUI frontend owns the visible window on Windows, so it (not only the
/// launcher) must carry the icon resource: GPUI loads the window/taskbar icon
/// from resource id 1 of its own executable. Without it a pinned shortcut shows
/// the generic Windows icon.
#[test]
fn windows_gpui_frontend_embeds_the_icon_resource() {
    let build = read("lan-mouse-gpui/build.rs");
    assert!(
        build.contains("assets/icon.ico"),
        "the frontend build script must embed assets/icon.ico"
    );
    assert!(
        build.contains("winresource"),
        "the frontend build script must use winresource"
    );
    let manifest = read("lan-mouse-gpui/Cargo.toml");
    assert!(
        manifest.contains("[target.'cfg(windows)'.build-dependencies]"),
        "the icon build dependency must be Windows-only"
    );
    assert!(
        manifest.contains("winresource"),
        "the frontend must declare the icon build dependency"
    );
    let lib = read("lan-mouse-gpui/src/lib.rs");
    assert!(
        lib.contains("Icon::from_resource"),
        "the Windows tray icon must come from the embedded resource"
    );
}

#[test]
fn deb_package_ships_icons_and_a_trusted_desktop_shortcut() {
    let script = read("scripts/package-deb.sh");

    assert!(
        script.contains("for size in 32 48 128 256 512; do"),
        "every hicolor size must be installed"
    );
    assert!(
        script.contains("assets/icon-${size}.png"),
        "the generated PNGs must be used"
    );
    assert!(
        script.contains("${size}x${size}/apps/de.feschber.LanMouse.png"),
        "icons are not installed into hicolor"
    );

    assert!(
        script.contains("usr/bin/lan-mouse-gpui"),
        "the deb must install the GPUI frontend binary"
    );
    assert!(script.contains("DEBIAN/postinst"), "postinst");
    assert!(script.contains("DEBIAN/prerm"), "prerm");
    assert!(
        script.contains("metadata::trusted true"),
        "the desktop shortcut must be trusted"
    );
    assert!(
        script.contains("xdg-user-dir DESKTOP"),
        "localized Desktop dir"
    );
    assert!(
        script.contains("gnome-shell-extension-appindicator"),
        "tray extension recommendation"
    );
}

#[test]
fn desktop_entry_points_at_the_installed_binary() {
    let entry = read("de.feschber.LanMouse.desktop");
    assert!(entry.contains("Exec=lan-mouse"), "exec");
    assert!(entry.contains("Icon=de.feschber.LanMouse"), "icon name");
    assert!(entry.contains("Type=Application"));
}

#[test]
fn icons_are_committed_and_valid() {
    assert!(exists("assets/icon.ico"), "windows icon");
    assert!(exists("assets/icon-256.png"), "package icon");
    assert!(
        exists("lan-mouse-gtk/resources/icons/icon-256.png"),
        "tray icon"
    );

    // ICO: reserved 0, type 1.
    let ico = fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icon.ico"))
        .expect("read icon.ico");
    assert_eq!(&ico[..4], &[0x00, 0x00, 0x01, 0x00]);

    // PNG signature.
    let png = fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icon-256.png"))
        .expect("read icon-256.png");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
}

#[test]
fn menu_offers_a_way_to_quit() {
    let ui = read("lan-mouse-gtk/resources/window.ui");
    assert!(
        ui.contains("app.quit-app"),
        "the hamburger menu must offer Quit Lan Mouse"
    );
}

#[test]
fn release_workflow_builds_both_platforms() {
    let workflow = read(".github/workflows/release.yml");
    assert!(workflow.contains("linux-release-build"));
    assert!(workflow.contains("windows-release-build"));
    assert!(workflow.contains("lan-mouse-custom-setup-x86_64.exe"));
    assert!(workflow.contains("lan-mouse-custom_*.deb"));
    // MSYS2 ships prebuilt GTK; gvsbuild needs a specific Visual Studio.
    assert!(workflow.contains("msys2/setup-msys2"));
    assert!(workflow.contains("msystem: UCRT64"));
    assert!(workflow.contains("windows-gpui-frontend"));
    assert!(workflow.contains("lan-mouse-gpui.exe"));
    assert!(workflow.contains("doctor"));
    assert!(workflow.contains("--probe"));
    assert!(workflow.contains("--json"));
}

#[test]
fn ci_workflow_tests_windows_generations_with_msys2() {
    let workflow = read(".github/workflows/rust.yml");
    assert!(workflow.contains("windows-2022"));
    assert!(workflow.contains("windows-2025"));
    assert!(workflow.contains("msys2/setup-msys2"));
    assert!(workflow.contains("msystem: UCRT64"));
}

#[test]
fn vm_test_lab_script_is_present_and_supports_win10_and_win11() {
    assert!(exists("scripts/vm-test-lab.sh"), "vm-test-lab.sh");
    let script = read("scripts/vm-test-lab.sh");
    assert!(script.contains("windows-10.conf"));
    assert!(script.contains("windows-11.conf"));
    assert!(script.contains("trusted_ips"));
}

#[test]
fn windows_installer_performs_a_reversible_clean_install_and_uninstall() {
    let script = read("installer/windows/lan-mouse-custom.nsi");

    // The two app-owned per-user directories are the only things touched.
    assert!(
        script.contains("!define LOCAL_USERDATA \"$LOCALAPPDATA\\lan-mouse\""),
        "config directory define"
    );
    assert!(
        script.contains("!define ROAMING_USERDATA \"$APPDATA\\lan-mouse\""),
        "roaming directory define"
    );

    // Clean = move to a timestamped sibling backup (reversible), not delete.
    assert!(
        script.contains("${GetTime}"),
        "timestamp for the backup name"
    );
    assert!(
        script.contains(".bak-$BackupStamp"),
        "timestamped backup name"
    );
    assert!(
        script.contains("Rename \"${LOCAL_USERDATA}\" \"$R7\""),
        "move the config directory to its backup"
    );
    assert!(
        script.contains("Rename \"${ROAMING_USERDATA}\" \"$R8\""),
        "move the roaming directory to its backup"
    );
    // /PURGEUSERDATA is the only path that actually deletes the data.
    assert!(script.contains("/PURGEUSERDATA"), "explicit purge switch");
    assert!(script.contains("PurgeLanMouseUserData"), "purge macro");
    assert!(
        script.contains("RMDir /r \"${LOCAL_USERDATA}\""),
        "purge deletes the app directory"
    );
    assert!(
        script.contains("RMDir /r \"${ROAMING_USERDATA}\""),
        "purge deletes the roaming directory"
    );

    // Clean install vs upgrade semantics and the escape hatches.
    assert!(script.contains("/KEEPUSERDATA"), "keep-config switch");
    assert!(script.contains("/CLEANUSERDATA"), "force-clean switch");
    assert!(
        script.contains("IfSilent"),
        "silent installs must not prompt"
    );
    assert!(script.contains("MB_YESNO"), "interactive prompt");
    assert!(
        script.contains("IfFileExists \"${LOCAL_USERDATA}\""),
        "detect leftovers before installing"
    );

    // Stop the GUI *and* the daemon it spawned before touching files.
    assert!(script.contains("taskkill /F /T /IM"), "stop GUI and daemon");

    // Do not mix binaries from older builds with the new ones.
    assert!(
        script.contains("StrCmp $0 \"$INSTDIR\""),
        "only wipe our own previous install directory"
    );

    // Upgrades must not accumulate duplicate firewall rules.
    assert!(
        script.contains("delete rule name=\"Lan Mouse\""),
        "remove stale firewall rule before adding the new one"
    );

    // Safety: never move/delete a shared/root directory.
    for forbidden in [
        "RMDir /r \"$LOCALAPPDATA\"",
        "RMDir /r \"$APPDATA\"",
        "RMDir /r \"$DESKTOP\"",
        "RMDir /r \"$SMPROGRAMS\"",
        "RMDir /r \"$SMSTARTUP\"",
        "RMDir /r \"$PROGRAMFILES64\"",
        "Rename \"$LOCALAPPDATA\"",
        "Rename \"$APPDATA\"",
    ] {
        assert!(
            !script.contains(forbidden),
            "installer must not touch {forbidden}"
        );
    }
}

#[test]
fn deb_postrm_keeps_user_data_and_documents_the_reset() {
    let script = read("scripts/package-deb.sh");
    assert!(script.contains("DEBIAN/postrm"), "postrm maintainer script");
    assert!(
        script.contains("lan-mouse --reset-config"),
        "purge must tell the user how to reset"
    );
    assert!(
        script.contains("--purge"),
        "purge must document the permanent-delete option"
    );
    assert!(
        script.contains("lan-mouse.bak-<timestamp>"),
        "purge must document the reversible backup location"
    );
    assert!(
        script.contains("per-user configuration was kept"),
        "purge explains that home data is intentionally preserved"
    );
    // Debian maintainer scripts must not wipe home directories.
    for forbidden in [
        "rm -rf /home",
        "rm -rf \"$home\"",
        "rm -rf $HOME",
        "rm -rf ~",
    ] {
        assert!(
            !script.contains(forbidden),
            "maintainer script must not run {forbidden}"
        );
    }
}

#[test]
fn clean_install_script_only_moves_or_purges_app_directories() {
    assert!(exists("scripts/install-clean.sh"), "install-clean.sh");
    let script = read("scripts/install-clean.sh");
    for needle in [
        "--dry-run",
        "--yes",
        "--purge",
        "--no-kill",
        "${XDG_CONFIG_HOME:-$HOME/.config}",
        "${XDG_STATE_HOME:-$HOME/.local/state}",
        "${XDG_CACHE_HOME:-$HOME/.cache}",
        "backup=\"${path}.bak-${stamp}\"",
        "mv -- \"$path\" \"$backup\"",
        "rm -rf -- \"$path\"",
        "refusing unexpected directory",
        "refusing path directly below the filesystem root",
    ] {
        assert!(script.contains(needle), "install-clean.sh missing {needle}");
    }
}

#[test]
fn fresh_install_documentation_lists_every_path() {
    let doc = read("docs/audit-fresh-install.md");
    for needle in [
        "%LOCALAPPDATA%\\lan-mouse",
        "%APPDATA%\\lan-mouse",
        "~/.config/lan-mouse",
        "~/.local/state/lan-mouse",
        "~/.cache/lan-mouse",
        "--reset-config",
        "--purge",
        "lan-mouse.bak-",
        "/KEEPUSERDATA",
        "/CLEANUSERDATA",
        "/PURGEUSERDATA",
    ] {
        assert!(doc.contains(needle), "fresh-install doc missing {needle}");
    }
}

#[test]
fn readme_documents_fresh_install_and_reset() {
    let readme = read("README.md");
    for needle in [
        "Clean install, reset and uninstall",
        "--reset-config",
        "--purge",
        "lan-mouse.bak-",
        "/KEEPUSERDATA",
        "/PURGEUSERDATA",
        "install-clean.sh",
    ] {
        assert!(readme.contains(needle), "README missing {needle}");
    }
    let doc = read("DOC.md");
    assert!(
        doc.contains("--reset-config"),
        "DOC.md must document the reset"
    );
}

#[test]
fn reset_config_moves_only_the_apps_own_directories_to_a_backup() {
    let env = FakeEnv::new("reset");
    let unrelated = env.create_stale_data();

    let output = run_reset(&env, &[]);
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("moved"),
        "reset reports what it moved: {stdout}"
    );
    assert!(
        stdout.contains("restore with:"),
        "reset explains how to restore the backup: {stdout}"
    );

    let dirs = env.app_data_dirs();
    for dir in &dirs {
        assert!(
            !dir.exists(),
            "{} must be moved away for a fresh start",
            dir.display()
        );
        let backups = backups_of(dir);
        assert_eq!(
            backups.len(),
            1,
            "expected one backup next to {}",
            dir.display()
        );
        assert!(
            backups[0].join("config.toml").exists(),
            "backup {} must contain the old configuration",
            backups[0].display()
        );
    }

    // Restoring the config directory brings the old data back (reversible).
    let config_dir = dirs.first().expect("config dir");
    let backup = backups_of(config_dir).into_iter().next().expect("backup");
    fs::rename(&backup, config_dir).expect("restore");
    assert!(config_dir.join("config.toml").exists());

    assert!(
        unrelated.join("important.txt").exists(),
        "unrelated data is kept"
    );
    let config_parent = config_dir.parent().expect("config parent");
    assert!(
        config_parent.exists(),
        "parent directories stay ({})",
        config_parent.display()
    );
}

#[test]
fn reset_config_dry_run_moves_nothing() {
    let env = FakeEnv::new("dry-run");
    let unrelated = env.create_stale_data();

    let output = run_reset(&env, &["--dry-run"]);
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("would move"),
        "dry run lists the backup destinations: {stdout}"
    );
    assert!(stdout.contains("nothing is moved or deleted") || stdout.contains("Dry run: nothing"));

    for dir in env.app_data_dirs() {
        assert!(dir.exists(), "{} must survive a dry run", dir.display());
        assert!(
            backups_of(&dir).is_empty(),
            "a dry run must not create a backup for {}",
            dir.display()
        );
    }
    assert!(unrelated.exists());
}

#[test]
fn reset_config_purge_deletes_without_a_backup() {
    let env = FakeEnv::new("purge");
    env.create_stale_data();

    let output = run_reset(&env, &["--purge"]);
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("permanently deleted"),
        "--purge reports the permanent deletion: {stdout}"
    );

    for dir in env.app_data_dirs() {
        assert!(
            !dir.exists(),
            "{} must be deleted by --purge",
            dir.display()
        );
        assert!(
            backups_of(&dir).is_empty(),
            "--purge must not leave a backup for {}",
            dir.display()
        );
    }
}

#[test]
fn reset_config_flags_require_the_reset_flag() {
    for flag in ["--dry-run", "--purge"] {
        let env = FakeEnv::new("requires");
        let output = env
            .command()
            .arg(flag)
            .output()
            .expect("run lan-mouse without --reset-config");
        assert!(!output.status.success(), "{flag} alone must be rejected");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--reset-config"),
            "clap must point at the missing flag for {flag}: {stderr}"
        );
    }
}

#[cfg(unix)]
#[test]
fn reset_config_refuses_while_the_service_is_running() {
    let env = FakeEnv::new("guard");
    env.create_stale_data();

    let socket_dir = env.path("runtime");
    fs::create_dir_all(&socket_dir).expect("socket dir");
    let socket = socket_dir.join("lan-mouse-socket.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("fake service socket");

    let output = run_reset(&env, &[]);
    assert!(
        !output.status.success(),
        "must refuse while the service runs"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("still running"), "clear refusal: {stderr}");
    for dir in env.app_data_dirs() {
        assert!(
            dir.exists(),
            "{} must not be moved while the service runs",
            dir.display()
        );
        assert!(backups_of(&dir).is_empty(), "no backup while running");
    }

    drop(listener);
    let _ = fs::remove_file(&socket);
    let output = run_reset(&env, &[]);
    assert_success(&output);
    for dir in env.app_data_dirs() {
        assert!(
            !dir.exists(),
            "{} must be moved once stopped",
            dir.display()
        );
        assert_eq!(backups_of(&dir).len(), 1, "one backup after the reset");
    }
}

#[test]
fn default_frontend_is_gpui() {
    let cargo = read("Cargo.toml");
    assert!(
        cargo.contains("default = [\n    \"gpui\",\n    \"layer_shell_capture\","),
        "the default frontend must be GPUI, not GTK"
    );
}
