//! Explicit reset of all per-user data Lan Mouse persists.
//!
//! Package installers and upgrades deliberately keep the user's devices,
//! certificates and permission policy; a *fresh* install must not resurrect
//! them. `lan-mouse --reset-config [--dry-run] [--purge]` is the explicit way
//! to get back to a pristine state:
//!
//! * config directory (`config.toml`: clients, devices, address rules, denied
//!   fingerprints and the certificate/private key),
//! * state directory (audit log and portal consent tokens),
//! * legacy cache directory (portal tokens written by older builds),
//! * on Windows `%APPDATA%\lan-mouse` (defensive: no current build writes
//!   there, but portable or older builds may have),
//! * on macOS the audit log directory (`~/Library/Logs/lan-mouse`).
//!
//! The default is **reversible**: every directory is renamed to a sibling
//! `lan-mouse.bak-<unix timestamp>` so a clean install hides the old devices
//! while the data can still be restored by renaming the backup back. Only
//! `--purge` deletes the data permanently.
//!
//! Nothing outside a directory literally named `lan-mouse` is ever touched,
//! symbolic links are moved without following them, and the command refuses to
//! run while the service still has the files open. The config directory is
//! resolved through [`crate::config::default_path`] so this cleanup can never
//! drift from where the app actually writes `config.toml`.

use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Name of every directory this application owns.
const APP_DIR_NAME: &str = "lan-mouse";

#[derive(Debug, thiserror::Error)]
pub(crate) enum ResetError {
    /// Refuse to race a running instance: it may rewrite the files we move
    /// (and on Windows the certificate is in use).
    #[error(
        "Lan Mouse is still running. Quit the app and the daemon, then run \
         `--reset-config` again."
    )]
    ServiceRunning,
    #[error("failed to move {path} to {backup}: {source}")]
    Move {
        path: PathBuf,
        backup: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to remove {path}: {source}")]
    Remove {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ResetOptions {
    /// Only report what would be moved/removed.
    pub dry_run: bool,
    /// Delete the data permanently instead of moving it to a backup.
    pub purge: bool,
}

/// Environment values relevant to the per-user data locations.
///
/// Kept separate from [`std::env`] so the path logic can be unit tested with
/// arbitrary values (and so tests never point at the real configuration).
#[derive(Clone, Debug, Default)]
struct ResetEnv {
    #[cfg(unix)]
    home: Option<OsString>,
    #[cfg(unix)]
    xdg_state_home: Option<OsString>,
    #[cfg(unix)]
    xdg_cache_home: Option<OsString>,
    #[cfg(windows)]
    app_data: Option<OsString>,
}

impl ResetEnv {
    fn from_process() -> Self {
        Self {
            #[cfg(unix)]
            home: env::var_os("HOME"),
            #[cfg(unix)]
            xdg_state_home: env::var_os("XDG_STATE_HOME"),
            #[cfg(unix)]
            xdg_cache_home: env::var_os("XDG_CACHE_HOME"),
            #[cfg(windows)]
            app_data: env::var_os("APPDATA"),
        }
    }
}

fn absolute(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

/// Per-user directories owned by Lan Mouse that are not part of the config
/// directory returned by [`crate::config::default_path`].
fn extra_dirs(env: &ResetEnv) -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    #[cfg(unix)]
    {
        let home = absolute(env.home.clone());
        let state_base = absolute(env.xdg_state_home.clone())
            .or_else(|| home.as_ref().map(|home| home.join(".local/state")));
        if let Some(base) = state_base {
            dirs.push(base.join(APP_DIR_NAME));
        }
        let cache_base = absolute(env.xdg_cache_home.clone())
            .or_else(|| home.as_ref().map(|home| home.join(".cache")));
        if let Some(base) = cache_base {
            dirs.push(base.join(APP_DIR_NAME));
        }
        #[cfg(target_os = "macos")]
        {
            if let Some(home) = home.as_ref() {
                dirs.push(home.join("Library").join("Logs").join(APP_DIR_NAME));
            }
        }
    }

    #[cfg(windows)]
    {
        // Current builds store everything in the config directory
        // (`%LOCALAPPDATA%\lan-mouse`, see `config::default_path`), but
        // portable or older builds may have written to the Roaming profile;
        // back that up as well.
        if let Some(base) = absolute(env.app_data.clone()) {
            dirs.push(base.join(APP_DIR_NAME));
        }
    }

    dirs
}

fn app_data_dirs(env: &ResetEnv) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(config_dir) = crate::config::default_path() {
        dirs.push(config_dir);
    }
    dirs.extend(extra_dirs(env));
    // `config::default_path` keeps a trailing separator; Windows refuses to
    // rename a directory whose path ends in a backslash, so normalize every
    // path through its components (which also collapses repeated separators).
    let dirs = dirs.into_iter().map(normalized_path).collect::<Vec<_>>();
    let mut seen = HashSet::new();
    dirs.into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

/// Lexically normalize a path (strip trailing separators and `.` components).
fn normalized_path(path: PathBuf) -> PathBuf {
    path.components().collect()
}

/// Whether `path` is a directory this app may move/delete.
///
/// The final component must be exactly [`APP_DIR_NAME`] and the path may not
/// sit directly at the filesystem root (e.g. `/lan-mouse`). This is the last
/// line of defence against touching a user-chosen or shared directory.
fn is_owned_app_dir(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    if path.file_name().and_then(|name| name.to_str()) != Some(APP_DIR_NAME) {
        return false;
    }
    // `/lan-mouse` has parent `/` which in turn has no parent.
    path.parent()
        .and_then(Path::parent)
        .is_some_and(|parent| !parent.as_os_str().is_empty())
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// Sibling path used to keep the data recoverable.
///
/// It lives in the same parent directory, so the rename is atomic and cannot
/// fail because of a filesystem boundary. A numeric suffix keeps repeated
/// resets in the same second from overwriting an earlier backup.
fn backup_path(path: &Path, timestamp: u64) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let base = format!("{APP_DIR_NAME}.bak-{timestamp}");
    let mut candidate = parent.join(&base);
    let mut suffix = 1u32;
    while candidate.exists() {
        candidate = parent.join(format!("{base}-{suffix}"));
        suffix += 1;
    }
    candidate
}

#[derive(Debug)]
enum Outcome {
    NotPresent,
    WouldMove(PathBuf),
    Moved(PathBuf),
    WouldPurge,
    Purged,
}

fn purge_path(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        // Never follow a symlink: remove the link itself, keep its target.
        fs::remove_file(path)
    } else if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Move (default) or permanently delete (`purge`) one app directory.
fn reset_path(path: &Path, options: ResetOptions, timestamp: u64) -> Result<Outcome, ResetError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Outcome::NotPresent),
        Err(source) => {
            return Err(ResetError::Remove {
                path: path.to_path_buf(),
                source,
            });
        }
    }

    if options.purge {
        if options.dry_run {
            return Ok(Outcome::WouldPurge);
        }
        purge_path(path).map_err(|source| ResetError::Remove {
            path: path.to_path_buf(),
            source,
        })?;
        return Ok(Outcome::Purged);
    }

    let backup = backup_path(path, timestamp);
    if options.dry_run {
        return Ok(Outcome::WouldMove(backup));
    }
    // Renaming keeps the data and is reversible; a symlink is moved as a link
    // (its target is never followed or modified).
    fs::rename(path, &backup).map_err(|source| ResetError::Move {
        path: path.to_path_buf(),
        backup: backup.clone(),
        source,
    })?;
    Ok(Outcome::Moved(backup))
}

fn describe_app_dir(path: &Path) -> &'static str {
    if path.ends_with("Library/Logs/lan-mouse") {
        "macOS audit log"
    } else if path.ends_with(".config/lan-mouse") {
        "configuration, devices, permissions and certificate"
    } else if path.ends_with(".local/state/lan-mouse") {
        "audit log and portal consent tokens"
    } else if path.ends_with(".cache/lan-mouse") {
        "legacy portal consent tokens"
    } else {
        "configuration, certificate, audit log and consent tokens"
    }
}

/// Move every per-user Lan Mouse directory to a backup (default) or delete it
/// (`--purge`); with `dry_run` only report what would happen.
///
/// Returns an error only when a move/removal actually failed; a directory that
/// does not exist is reported as such and is not an error, so the command is
/// idempotent.
pub(crate) fn run(options: ResetOptions) -> Result<(), ResetError> {
    if lan_mouse_ipc::is_service_running() {
        return Err(ResetError::ServiceRunning);
    }

    let env = ResetEnv::from_process();
    let dirs = app_data_dirs(&env);
    println!(
        "lan-mouse: {}",
        if options.dry_run {
            "checking per-user data (dry run, nothing is moved or deleted)"
        } else if options.purge {
            "permanently deleting per-user data (--purge)"
        } else {
            "moving per-user data to a backup (reversible)"
        }
    );
    if dirs.is_empty() {
        println!("lan-mouse: could not resolve any Lan Mouse data directory");
        return Ok(());
    }

    let timestamp = unix_timestamp();
    let mut backed_up = 0;
    let mut purged = 0;
    let mut skipped = 0;
    let mut failure = None;
    for path in &dirs {
        if !is_owned_app_dir(path) {
            println!(
                "lan-mouse: refusing to touch {} (not a lan-mouse data directory)",
                path.display()
            );
            skipped += 1;
            continue;
        }
        match reset_path(path, options, timestamp) {
            Ok(Outcome::WouldMove(backup)) => {
                println!(
                    "lan-mouse: would move {} ({}) to {}",
                    path.display(),
                    describe_app_dir(path),
                    backup.display()
                );
                backed_up += 1;
            }
            Ok(Outcome::Moved(backup)) => {
                println!(
                    "lan-mouse: moved {} ({}) to {}",
                    path.display(),
                    describe_app_dir(path),
                    backup.display()
                );
                println!(
                    "lan-mouse:   restore with: mv \"{}\" \"{}\"",
                    backup.display(),
                    path.display()
                );
                backed_up += 1;
            }
            Ok(Outcome::WouldPurge) => {
                println!(
                    "lan-mouse: would permanently delete {} ({}) [--purge]",
                    path.display(),
                    describe_app_dir(path)
                );
                purged += 1;
            }
            Ok(Outcome::Purged) => {
                println!(
                    "lan-mouse: permanently deleted {} ({}) [--purge]",
                    path.display(),
                    describe_app_dir(path)
                );
                purged += 1;
            }
            Ok(Outcome::NotPresent) => {
                println!("lan-mouse: not present {}", path.display());
            }
            Err(error) => {
                println!("lan-mouse: {error}");
                skipped += 1;
                failure.get_or_insert(error);
            }
        }
    }

    #[cfg(target_os = "macos")]
    println!(
        "lan-mouse: note: macOS accessibility/input-monitoring permissions are managed by the OS \
         and are not reset by this command (see System Settings → Privacy & Security)."
    );
    #[cfg(not(unix))]
    println!(
        "lan-mouse: note: this only moves per-user files; uninstall Lan Mouse normally to remove \
         the installed binaries, shortcuts and firewall rule as well."
    );
    if !options.purge {
        println!(
            "lan-mouse: note: backups named `{APP_DIR_NAME}.bak-*` are kept until you delete them."
        );
    }

    if options.dry_run {
        println!(
            "lan-mouse: dry run finished, {backed_up} directory/directories would be backed up, \
             {purged} would be permanently deleted"
        );
    } else {
        println!(
            "lan-mouse: done, {backed_up} directory/directories backed up, {purged} permanently \
             deleted, {skipped} skipped"
        );
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "lan-mouse-reset-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn only_directories_literally_named_lan_mouse_are_owned() {
        assert!(is_owned_app_dir(Path::new("/home/user/.config/lan-mouse")));
        assert!(is_owned_app_dir(Path::new("/tmp/lan-mouse")));
        assert!(!is_owned_app_dir(Path::new("/home/user/.config/other")));
        assert!(!is_owned_app_dir(Path::new("/lan-mouse")));
        assert!(!is_owned_app_dir(Path::new("/")));
        assert!(!is_owned_app_dir(Path::new("relative/lan-mouse")));
        assert!(!is_owned_app_dir(Path::new("/home/lan-mouse/.config")));
    }

    #[test]
    fn trailing_separators_are_normalized_before_moving() {
        assert_eq!(
            normalized_path(PathBuf::from("/home/user/.config/lan-mouse/")),
            PathBuf::from("/home/user/.config/lan-mouse")
        );
        assert_eq!(
            normalized_path(PathBuf::from("/home/user//lan-mouse")),
            PathBuf::from("/home/user/lan-mouse")
        );
    }

    #[test]
    fn backup_names_are_unique_siblings() {
        let root = unique_temp_dir("backup-name");
        let app_dir = root.join("lan-mouse");
        fs::create_dir_all(&app_dir).expect("app dir");

        let first = backup_path(&app_dir, 1_700_000_000);
        assert_eq!(
            first,
            root.join("lan-mouse.bak-1700000000"),
            "same parent, timestamped name"
        );
        fs::create_dir_all(&first).expect("existing backup");
        let second = backup_path(&app_dir, 1_700_000_000);
        assert_eq!(second, root.join("lan-mouse.bak-1700000000-1"));

        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn default_reset_moves_data_to_a_reversible_backup() {
        let root = unique_temp_dir("move");
        let app_dir = root.join("lan-mouse");
        fs::create_dir_all(&app_dir).expect("app dir");
        fs::write(app_dir.join("config.toml"), "stale devices").expect("file");

        let outcome =
            reset_path(&app_dir, ResetOptions::default(), 1_700_000_000).expect("move to backup");
        let Outcome::Moved(backup) = outcome else {
            panic!("expected a reversible move, got {outcome:?}");
        };
        assert!(!app_dir.exists(), "old path must be gone (fresh state)");
        assert_eq!(
            fs::read_to_string(backup.join("config.toml")).expect("backup content"),
            "stale devices",
            "the data must be preserved in the backup"
        );

        // Restoring is just renaming the backup back.
        fs::rename(&backup, &app_dir).expect("restore");
        assert!(app_dir.join("config.toml").exists());

        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn dry_run_reports_the_backup_without_moving() {
        let root = unique_temp_dir("dry");
        let app_dir = root.join("lan-mouse");
        fs::create_dir_all(&app_dir).expect("app dir");
        fs::write(app_dir.join("config.toml"), "stale").expect("file");

        let options = ResetOptions {
            dry_run: true,
            purge: false,
        };
        let outcome = reset_path(&app_dir, options, 1_700_000_000).expect("dry run");
        let Outcome::WouldMove(backup) = outcome else {
            panic!("expected WouldMove, got {outcome:?}");
        };
        assert!(backup.to_string_lossy().contains("lan-mouse.bak-"));
        assert!(app_dir.join("config.toml").exists());
        assert!(!backup.exists());

        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn purge_deletes_without_creating_a_backup() {
        let root = unique_temp_dir("purge");
        let app_dir = root.join("lan-mouse");
        fs::create_dir_all(app_dir.join("nested")).expect("nested");
        fs::write(app_dir.join("config.toml"), "stale").expect("file");

        let options = ResetOptions {
            dry_run: false,
            purge: true,
        };
        let outcome = reset_path(&app_dir, options, 1_700_000_000).expect("purge");
        assert!(matches!(outcome, Outcome::Purged));
        assert!(!app_dir.exists());
        assert!(
            fs::read_dir(&root).expect("root").all(|entry| !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .contains(".bak-")),
            "--purge must not leave a backup behind"
        );

        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn resetting_a_missing_directory_is_not_an_error() {
        let root = unique_temp_dir("missing");
        let app_dir = root.join("lan-mouse");
        let outcome =
            reset_path(&app_dir, ResetOptions::default(), 1_700_000_000).expect("missing is fine");
        assert!(matches!(outcome, Outcome::NotPresent));
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_moved_without_touching_their_target() {
        let root = unique_temp_dir("symlink");
        let target = root.join("real-data");
        fs::create_dir_all(&target).expect("target");
        fs::write(target.join("config.toml"), "keep me").expect("file");
        let link = root.join("lan-mouse");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let outcome =
            reset_path(&link, ResetOptions::default(), 1_700_000_000).expect("move link to backup");
        assert!(matches!(outcome, Outcome::Moved(_)));
        assert!(!link.exists());
        assert!(target.join("config.toml").exists());

        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn unix_extra_dirs_follow_xdg_overrides() {
        let env = ResetEnv {
            home: Some(OsString::from("/home/example")),
            xdg_state_home: Some(OsString::from("/xdg/state")),
            xdg_cache_home: Some(OsString::from("/xdg/cache")),
        };
        let dirs = extra_dirs(&env);
        assert!(dirs.contains(&PathBuf::from("/xdg/state/lan-mouse")));
        assert!(dirs.contains(&PathBuf::from("/xdg/cache/lan-mouse")));
    }

    #[cfg(unix)]
    #[test]
    fn unix_extra_dirs_fall_back_to_home() {
        let env = ResetEnv {
            home: Some(OsString::from("/home/example")),
            ..Default::default()
        };
        let dirs = extra_dirs(&env);
        assert!(dirs.contains(&PathBuf::from("/home/example/.local/state/lan-mouse")));
        assert!(dirs.contains(&PathBuf::from("/home/example/.cache/lan-mouse")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_extra_dirs_cover_roaming_appdata() {
        let env = ResetEnv {
            app_data: Some(OsString::from(r"C:\Users\example\AppData\Roaming")),
        };
        let dirs = extra_dirs(&env);
        assert!(dirs.contains(&PathBuf::from(
            r"C:\Users\example\AppData\Roaming\lan-mouse"
        )));
    }
}
