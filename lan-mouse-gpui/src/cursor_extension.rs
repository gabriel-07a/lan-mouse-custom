//! Optional GNOME Shell extension that hides the local cursor while a peer is
//! being controlled on Wayland, where a normal client cannot hide the
//! compositor cursor.
//!
//! The app bundles the extension source and installs it under the user's
//! `gnome-shell/extensions` directory. GNOME Wayland only discovers a newly
//! installed extension after a logout/login, so the UI tells the user that.

use std::{fs, path::PathBuf, process::Command};

pub(crate) const UUID: &str = "lan-mouse-cursor@lan-mouse-custom";

const EXTENSION_JS: &str =
    include_str!("../../linux/gnome-extension/lan-mouse-cursor@lan-mouse-custom/extension.js");
const METADATA_JSON: &str =
    include_str!("../../linux/gnome-extension/lan-mouse-cursor@lan-mouse-custom/metadata.json");

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CursorExtensionState {
    /// Only relevant on GNOME Wayland; elsewhere the row is hidden.
    pub needed: bool,
    pub installed: bool,
    pub enabled: bool,
    /// Extra instruction for the user (logout/login).
    pub hint: Option<String>,
}

/// Detect the current state without changing anything.
pub(crate) fn detect() -> CursorExtensionState {
    let needed = is_gnome_wayland();
    if !needed {
        return CursorExtensionState::default();
    }
    let installed = extension_installed();
    let enabled = enabled_extensions()
        .is_some_and(|extensions| extensions.iter().any(|extension| extension == UUID));
    let loaded = extension_loaded();
    CursorExtensionState {
        needed,
        installed,
        enabled,
        hint: (installed && !loaded || !installed).then(|| "logout/login".to_string()),
    }
}

/// Install the bundled extension for the current user and enable it.
pub(crate) fn install() -> Result<CursorExtensionState, String> {
    let user_dir = user_extension_dir()
        .ok_or_else(|| "could not resolve the user data directory".to_string())?;
    fs::create_dir_all(&user_dir)
        .map_err(|error| format!("could not create {}: {error}", user_dir.display()))?;
    for (name, contents) in [
        ("extension.js", EXTENSION_JS),
        ("metadata.json", METADATA_JSON),
    ] {
        let path = user_dir.join(name);
        fs::write(&path, contents)
            .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    }

    let mut extensions = enabled_extensions().unwrap_or_default();
    if !extensions.iter().any(|extension| extension == UUID) {
        extensions.push(UUID.to_string());
        set_enabled_extensions(&extensions)?;
    }
    let mut state = detect();
    state.installed = true;
    state.enabled = true;
    state.hint = Some("logout/login".to_string());
    Ok(state)
}

/// GNOME Wayland is the only setup where this extension is needed.
fn is_gnome_wayland() -> bool {
    let wayland = std::env::var("XDG_SESSION_TYPE")
        .is_ok_and(|session| session.eq_ignore_ascii_case("wayland"));
    let gnome = std::env::var("XDG_CURRENT_DESKTOP")
        .is_ok_and(|desktop| desktop.to_ascii_uppercase().contains("GNOME"));
    wayland && gnome
}

fn user_extension_dir() -> Option<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    Some(data_home.join("gnome-shell/extensions").join(UUID))
}

/// The extension creates this marker from `enable()`. It is absent while the
/// extension is only listed in GSettings but the running Shell has not loaded
/// it yet (the Wayland logout/login window).
fn extension_loaded() -> bool {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .is_some_and(|runtime| runtime.join("lan-mouse/cursor-extension-loaded").is_file())
}

fn extension_installed() -> bool {
    user_extension_dir().is_some_and(|dir| dir.join("extension.js").is_file())
        || PathBuf::from("/usr/share/gnome-shell/extensions")
            .join(UUID)
            .join("extension.js")
            .is_file()
}

fn enabled_extensions() -> Option<Vec<String>> {
    let output = Command::new("gsettings")
        .args(["get", "org.gnome.shell", "enabled-extensions"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_gsettings_array(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn set_enabled_extensions(extensions: &[String]) -> Result<(), String> {
    let value = format_gsettings_array(extensions);
    let output = Command::new("gsettings")
        .args(["set", "org.gnome.shell", "enabled-extensions", &value])
        .output()
        .map_err(|error| format!("could not run gsettings: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "gsettings failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Parse `gsettings get org.gnome.shell enabled-extensions` output.
///
/// Example: `['ubuntu-dock@ubuntu.com', 'ding@rastersoft.com']`.
fn parse_gsettings_array(input: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for character in input.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '\'' => {
                if quoted {
                    result.push(std::mem::take(&mut current));
                }
                quoted = !quoted;
            }
            _ if quoted => current.push(character),
            _ => {}
        }
    }
    result
}

fn format_gsettings_array(extensions: &[String]) -> String {
    let quoted = extensions
        .iter()
        .map(|extension| format!("'{}'", extension.replace('\\', "\\\\").replace('\'', "\\'")))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{quoted}]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_existing_extension_list() {
        assert_eq!(
            parse_gsettings_array("['ubuntu-dock@ubuntu.com', 'ding@rastersoft.com']"),
            vec!["ubuntu-dock@ubuntu.com", "ding@rastersoft.com"]
        );
        assert_eq!(parse_gsettings_array("@as []"), Vec::<String>::new());
    }

    #[test]
    fn formatting_round_trips_through_the_parser() {
        let extensions = vec!["ubuntu-dock@ubuntu.com".to_string(), UUID.to_string()];
        let formatted = format_gsettings_array(&extensions);
        assert_eq!(parse_gsettings_array(&formatted), extensions);
    }

    #[test]
    fn detection_is_hidden_outside_gnome_wayland() {
        // CI usually has no XDG_CURRENT_DESKTOP/XDG_SESSION_TYPE; if it does,
        // the important invariant is that a non-GNOME or non-Wayland session
        // never asks for the extension.
        if !is_gnome_wayland() {
            assert!(!detect().needed);
        }
    }
}
