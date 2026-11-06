//! Guards for the pieces that make the desktop integration work but cannot be
//! exercised by a normal unit test: the installer scripts, the packaged icon
//! and the desktop entry.
//!
//! These are cheap string checks, but they catch the regressions that hurt
//! most: a shortcut that loses its icon, a .deb that stops shipping the icon
//! theme files, or a package that forgets to remove the desktop shortcut.

use std::{fs, path::Path};

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn exists(relative: &str) -> bool {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(relative)
        .exists()
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
