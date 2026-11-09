//! Persisted consent tokens for the xdg-desktop-portal RemoteDesktop API.
//!
//! A restore token is the user's persisted consent ("Remember This
//! Selection"). The portal invalidates it after every successful start and
//! returns a replacement, so it is rewritten on each start. Tokens are bearer
//! capabilities: whoever can read one can restore the portal session without a
//! permission dialog, hence 0600 for the file and 0700 for the directory.
//!
//! Shared by the libei backend and the \`xdp\` fallback; each keeps its own
//! token file because the tokens are single-use and bound to the session that
//! issued them.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

/// Name of the RemoteDesktop restore token file used by the libei backend.
#[cfg_attr(not(libei), allow(dead_code))]
pub(crate) const REMOTE_DESKTOP_TOKEN_FILE: &str = "remote-desktop.token";

/// Directory holding portal tokens: \`$XDG_STATE_HOME/lan-mouse\`, falling back
/// to \`~/.local/state/lan-mouse\`.
///
/// State, not cache: cache directories are routinely purged by cleanup tools
/// and losing the token makes the portal ask for permission again.
fn token_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))?;
    Some(base.join("lan-mouse"))
}

/// Token location used by builds that stored consent in the cache directory.
fn legacy_token_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("lan-mouse"))
}

fn read_token_file(path: &Path) -> Option<String> {
    let token = fs::read_to_string(path).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Read the saved consent token \`name\`, migrating a token left in the legacy
/// cache directory so an already-granted consent survives the move.
fn read_token_from(
    state_dir: Option<&Path>,
    legacy_dir: Option<&Path>,
    name: &str,
) -> Option<String> {
    let state_dir = state_dir?;
    let path = state_dir.join(name);
    if let Some(token) = read_token_file(&path) {
        return Some(token);
    }
    let legacy_path = legacy_dir?.join(name);
    let token = read_token_file(&legacy_path)?;
    log::info!(
        "migrating portal restore token from {} to {}",
        legacy_path.display(),
        path.display()
    );
    if let Err(e) = write_token_to(state_dir, name, &token) {
        log::warn!("could not migrate the portal restore token: {e}");
    } else if let Err(e) = fs::remove_file(&legacy_path) {
        // Do not keep a world-readable copy of a bearer capability around.
        log::debug!("could not remove the legacy restore token: {e}");
    }
    Some(token)
}

/// Read a persisted portal consent token by file name.
pub(crate) fn read_named_token(name: &str) -> Option<String> {
    read_token_from(token_dir().as_deref(), legacy_token_dir().as_deref(), name)
}

/// Remove a persisted consent token (best effort).
pub(crate) fn remove_named_token(name: &str) {
    if let Some(dir) = token_dir() {
        let _ = fs::remove_file(dir.join(name));
    }
    if let Some(dir) = legacy_token_dir() {
        let _ = fs::remove_file(dir.join(name));
    }
}

/// Save a consent token atomically with owner-only permissions.
pub(crate) fn write_named_token(name: &str, token: &str) -> io::Result<()> {
    let dir = token_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "neither XDG_STATE_HOME nor HOME is set",
        )
    })?;
    write_token_to(&dir, name, token)
}

fn write_token_to(dir: &Path, name: &str, token: &str) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;

    let path = dir.join(name);
    // Write next to the destination and rename: a crash mid-write must not
    // leave a truncated token that would then be sent back to the portal.
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    // The mode above only applies when the file is created; tighten an
    // existing file as well.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod token_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "lan-mouse-emulation-token-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn tokens_are_written_atomically_with_owner_only_permissions() {
        let dir = scratch_dir("write");
        write_token_to(&dir, REMOTE_DESKTOP_TOKEN_FILE, "token-value").unwrap();

        let path = dir.join(REMOTE_DESKTOP_TOKEN_FILE);
        assert_eq!(read_token_file(&path).as_deref(), Some("token-value"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // The temporary file is renamed, not left behind.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_tokens_are_treated_as_missing() {
        let dir = scratch_dir("empty");
        write_token_to(&dir, REMOTE_DESKTOP_TOKEN_FILE, " \n\t").unwrap();
        assert_eq!(read_token_file(&dir.join(REMOTE_DESKTOP_TOKEN_FILE)), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn legacy_cache_token_is_migrated_once() {
        let state = scratch_dir("state");
        let legacy = scratch_dir("legacy");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join(REMOTE_DESKTOP_TOKEN_FILE), "legacy-token").unwrap();

        assert_eq!(
            read_token_from(Some(&state), Some(&legacy), REMOTE_DESKTOP_TOKEN_FILE).as_deref(),
            Some("legacy-token")
        );
        assert_eq!(
            read_token_file(&state.join(REMOTE_DESKTOP_TOKEN_FILE)).as_deref(),
            Some("legacy-token")
        );
        // The legacy copy is removed once migration succeeded.
        assert!(!legacy.join(REMOTE_DESKTOP_TOKEN_FILE).exists());

        // The migrated (state) token wins over a stale legacy one.
        fs::write(state.join(REMOTE_DESKTOP_TOKEN_FILE), "current-token").unwrap();
        fs::write(legacy.join(REMOTE_DESKTOP_TOKEN_FILE), "legacy-token").unwrap();
        assert_eq!(
            read_token_from(Some(&state), Some(&legacy), REMOTE_DESKTOP_TOKEN_FILE).as_deref(),
            Some("current-token")
        );

        fs::remove_dir_all(&state).unwrap();
        fs::remove_dir_all(&legacy).unwrap();
    }

    #[test]
    fn different_portal_backends_use_different_token_files() {
        let dir = scratch_dir("named");
        write_token_to(&dir, "remote-desktop.token", "libei-token").unwrap();
        write_token_to(&dir, "remote-desktop-xdp.token", "xdp-token").unwrap();
        assert_eq!(
            read_token_file(&dir.join("remote-desktop.token")).as_deref(),
            Some("libei-token")
        );
        assert_eq!(
            read_token_file(&dir.join("remote-desktop-xdp.token")).as_deref(),
            Some("xdp-token")
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
