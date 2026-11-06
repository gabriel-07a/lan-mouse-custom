//! QA regression tests for the clean-install requirement (bug 4).
//!
//! User report: after installing the v0.2.4-custom release over previous test
//! builds, old configured PCs, certificates and trust entries were still
//! shown. The requirement is that a fresh install starts with no configured
//! devices, **without destroying user data**: the installer now moves the old
//! per-user state to a timestamped `lan-mouse.bak-<time>` backup (reversible),
//! asks first in interactive mode, and only deletes it with an explicit
//! `/PURGEUSERDATA` (or `lan-mouse --reset-config --purge` on the CLI).
//!
//! These tests guard the installer script (task-20, gtk-phase1 owns the
//! NSIS/README changes; this file belongs to QA task-21). The real install
//! matrix on Windows 10/11 is manual and documented in `docs/qa-repro-matrix.md`.

use std::fs;
use std::path::Path;

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn nsi() -> String {
    read("installer/windows/lan-mouse-custom.nsi")
}

fn lower_section<'a>(script: &'a str, marker: &str) -> &'a str {
    let start = script
        .find(marker)
        .unwrap_or_else(|| panic!("installer has no {marker}"));
    &script[start..]
}

fn macro_body<'a>(script: &'a str, name: &str) -> &'a str {
    let start = script
        .find(&format!("!macro {name}"))
        .unwrap_or_else(|| panic!("installer has no {name} macro"));
    let rest = &script[start..];
    let end = rest
        .find("!macroend")
        .unwrap_or_else(|| panic!("{name} macro has no end"));
    &rest[..end]
}

/// Only the two directories the app owns may ever be touched: the backups
/// live next to them with a `lan-mouse.bak-` suffix and the permanent purge
/// is explicit. No generic profile directory may be deleted.
#[test]
fn installer_only_ever_touches_lan_mouse_directories() {
    let script = nsi();
    let lower = script.to_lowercase();

    assert!(
        lower.contains(r#"!define local_userdata "$localappdata\lan-mouse""#),
        "installer deve apontar o estado para %LOCALAPPDATA%\\lan-mouse"
    );
    assert!(
        lower.contains(r#"!define roaming_userdata "$appdata\lan-mouse""#),
        "installer deve cobrir o legado %APPDATA%\\lan-mouse"
    );
    assert!(
        lower.contains("lan-mouse.bak-"),
        "backup do estado deve ser um diretório lan-mouse.bak-<timestamp> ao lado"
    );

    // The reversible clean-install macro must MOVE the dirs, never delete them.
    let remove = macro_body(&script, "RemoveLanMouseUserData").to_lowercase();
    assert!(
        remove.contains("rename") && remove.contains(".bak-"),
        "limpeza padrão deve usar Rename para o backup (reversível)"
    );
    assert!(
        !remove.contains("rmdir"),
        "limpeza padrão não pode apagar o estado do usuário (só --purge pode)"
    );

    for forbidden in [
        "rmdir /r \"$localappdata\"",
        "rmdir /r \"$appdata\"",
        "rmdir /r \"$userprofile\"",
        "rmdir /r \"$desktop\"",
        "rmdir /r \"$documents\"",
    ] {
        assert!(
            !lower.contains(forbidden),
            "instalador não pode apagar diretório genérico do usuário: {forbidden}"
        );
    }
}

/// A fresh install must not keep old devices: interactive install asks first
/// (default Yes moves the old config to a backup), silent `/S` moves it by
/// default, `/KEEPUSERDATA` keeps it (upgrade) and `/CLEANUSERDATA` forces
/// the fresh start. `--purge`/`/PURGEUSERDATA` is the only permanent delete.
#[test]
fn fresh_install_moves_stale_state_to_backup() {
    let script = nsi();
    let lower = script.to_lowercase();
    let on_init = lower_section(&lower, "function .oninit");

    assert!(
        lower.contains("keepuserdata") && lower.contains("cleanuserdata"),
        "instalador deve expor /KEEPUSERDATA e /CLEANUSERDATA para deploy silencioso"
    );
    assert!(
        lower.contains("purgeuserdata"),
        "deleção permanente deve exigir /PURGEUSERDATA explícito"
    );
    assert!(
        on_init.contains("removelanmouseuserdata"),
        "instalação limpa deve mover o estado antigo no .onInit"
    );
    assert!(
        lower.contains("restored by renaming the backup back"),
        "o prompt deve explicar que o backup é reversível (renomear de volta)"
    );
}

/// Uninstalling moves the configured devices to a backup by default (silent)
/// or after confirmation (interactive), keeping `/KEEPUSERDATA` (leave in
/// place) and `/PURGEUSERDATA` (delete) available.
#[test]
fn uninstall_moves_user_state_to_backup_by_default() {
    let script = nsi();
    let lower = script.to_lowercase();
    let uninstall = lower_section(&lower, "section \"uninstall\"");

    assert!(
        uninstall.contains("removelanmouseuserdata"),
        "desinstalação deve criar backup do config/cert/audit por padrão"
    );
    assert!(
        uninstall.contains("purgelanmouseuserdata"),
        "desinstalação deve permitir deleção permanente só com /PURGEUSERDATA"
    );
    assert!(
        uninstall.contains("keepuserdata"),
        "desinstalação deve suportar /KEEPUSERDATA para preservar o estado"
    );
    assert!(
        uninstall.contains("messagebox") && uninstall.contains("ifsilent"),
        "desinstalação interativa deve perguntar; silenciosa decide pelo /KEEPUSERDATA"
    );
    assert!(
        uninstall.contains("backup"),
        "o prompt de desinstalação deve mencionar o backup"
    );
}
