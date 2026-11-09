//! Append-only audit log for connection and permission events.
//!
//! Every grant, revoke, pairing window, address-rule change and session
//! lifecycle change is written as one JSON object per line to
//! `$XDG_STATE_HOME/lan-mouse/audit.jsonl` (or the platform equivalent).
//! The log is bounded by rotation: when it grows past [`MAX_SIZE`] it is
//! renamed to `audit.jsonl.1` and a fresh file is started.
//!
//! Persisted history answers questions the UI cannot: "who controlled this
//! machine yesterday", "when was this device revoked", "which devices did the
//! pairing window adopt". It is deliberately simple: no database, no
//! secrets (fingerprints, names and addresses only).
//!
//! The line format is append-only and additive: readers deserialize with
//! serde, which ignores unknown fields, and [`AuditLog::recent`] skips lines
//! it cannot parse. Ordering is the append order (the file is never sorted by
//! timestamp, so a wall-clock adjustment cannot reorder history).

use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Rotate the log once it grows past this size.
pub(crate) const MAX_SIZE: u64 = 5 * 1024 * 1024;

/// Upper bound for the in-memory map used by [`AuditLog::event_state_change`].
///
/// The helper is opt-in for call sites that re-emit the same state (the
/// service-side peer_permissions call is being moved to it); until that call
/// site lands the constant and helpers are only exercised by the tests.
#[cfg_attr(not(test), allow(dead_code))]
const MAX_TRACKED_CHANGES: usize = 256;

/// One line of the audit log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AuditRecord {
    /// unix timestamp in seconds
    pub ts: u64,
    /// stable machine readable event name (`grant_created`, `revoke`, ...)
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    /// free-form detail (source, capabilities, reason)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Default)]
struct AuditState {
    /// Whether the parent directory is known to exist, so the common append
    /// path does not need a create_dir_all syscall per record.
    parent_ready: bool,
    /// Whether the current file permissions were tightened already. Only the
    /// unix path tightens permissions; keeping the field gated avoids a
    /// "never read" warning on Windows (CI builds with -D warnings).
    #[cfg(unix)]
    file_tightened: bool,
    /// Last write time per state-change key (see event_state_change).
    #[cfg_attr(not(test), allow(dead_code))]
    last_written: HashMap<String, Instant>,
}

/// Append-only writer/reader for the audit log.
#[derive(Debug)]
pub(crate) struct AuditLog {
    path: PathBuf,
    state: RefCell<AuditState>,
}

impl AuditLog {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: RefCell::new(AuditState::default()),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Platform default location (overridable with `LAN_MOUSE_AUDIT_LOG`,
    /// which the tests use to stay out of the real state directory).
    pub(crate) fn default_path() -> PathBuf {
        if let Ok(path) = std::env::var("LAN_MOUSE_AUDIT_LOG") {
            return PathBuf::from(path);
        }
        #[cfg(target_os = "linux")]
        {
            let state = std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
                })
                .unwrap_or_else(|| PathBuf::from("."));
            return state.join("lan-mouse").join("audit.jsonl");
        }
        #[cfg(target_os = "macos")]
        {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            return PathBuf::from(home)
                .join("Library/Logs/lan-mouse")
                .join("audit.jsonl");
        }
        #[cfg(windows)]
        {
            let data = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
            return PathBuf::from(data).join("lan-mouse").join("audit.jsonl");
        }
        #[allow(unreachable_code)]
        PathBuf::from("audit.jsonl")
    }

    /// Append one record. Failures are logged and never break the service:
    /// auditing must not take input sharing down.
    pub(crate) fn event(
        &self,
        event: &str,
        fingerprint: Option<&str>,
        name: Option<&str>,
        addr: Option<&str>,
        detail: Option<&str>,
    ) {
        if let Err(e) = self.append(&record(event, fingerprint, name, addr, detail)) {
            log::warn!("failed to write audit log {:?}: {e}", self.path);
        }
    }

    /// Append a record only when the state it describes actually changed.
    ///
    /// Some peer events (permission snapshots, repeated authentication
    /// requests) are re-emitted every few seconds, which used to fill the log
    /// with thousands of identical rows. The caller supplies a key that
    /// encodes the *state* (fingerprint plus capabilities, never the ephemeral
    /// source port): identical states inside `window` are suppressed, while a
    /// real state change always produces a record.
    ///
    /// Returns whether a record was written.
    #[cfg_attr(not(test), allow(dead_code))]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn event_state_change(
        &self,
        key: &str,
        window: Duration,
        event: &str,
        fingerprint: Option<&str>,
        name: Option<&str>,
        addr: Option<&str>,
        detail: Option<&str>,
    ) -> bool {
        let now = Instant::now();
        if !self.allows_write(key, window, now) {
            log::trace!("audit: suppressed repeated {event} for the same state");
            return false;
        }
        match self.append(&record(event, fingerprint, name, addr, detail)) {
            Ok(()) => {
                self.mark_written(key, now);
                true
            }
            Err(e) => {
                // Do not suppress the next attempt: the record was not written.
                log::warn!("failed to write audit log {:?}: {e}", self.path);
                false
            }
        }
    }

    /// Whether a record for `key` may be written now. Kept separate from the
    /// clock so it can be unit tested with synthetic instants.
    #[cfg_attr(not(test), allow(dead_code))]
    fn allows_write(&self, key: &str, window: Duration, now: Instant) -> bool {
        let state = self.state.borrow();
        !state
            .last_written
            .get(key)
            .is_some_and(|last| now.duration_since(*last) < window)
    }

    /// Remember that a state-change record for `key` was written.
    #[cfg_attr(not(test), allow(dead_code))]
    fn mark_written(&self, key: &str, now: Instant) {
        let mut state = self.state.borrow_mut();
        if state.last_written.len() >= MAX_TRACKED_CHANGES {
            // Evict the oldest key; the map is small, so this is cheap.
            if let Some(oldest) = state
                .last_written
                .iter()
                .min_by_key(|(_, last)| **last)
                .map(|(key, _)| key.clone())
            {
                state.last_written.remove(&oldest);
            }
        }
        state.last_written.insert(key.to_string(), now);
    }

    fn append(&self, record: &AuditRecord) -> io::Result<()> {
        self.prepare_path()?;
        self.rotate_if_needed();
        let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        #[cfg(unix)]
        self.tighten_permissions(&file);
        // A single write keeps the record atomic for concurrent readers and
        // cannot interleave a line with its terminator when another writer
        // appends at the same time (CLI history is read through the service,
        // but older builds and doctor may read the file directly).
        file.write_all(&line)
    }

    /// Create the parent directory once, restricting its permissions on unix.
    fn prepare_path(&self) -> io::Result<()> {
        let Some(parent) = self.path.parent() else {
            return Ok(());
        };
        if self.state.borrow().parent_ready {
            return Ok(());
        }
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The log holds device names, addresses and fingerprints; it is
            // not world readable even though it holds no secrets.
            if let Err(e) = fs::set_permissions(parent, fs::Permissions::from_mode(0o700)) {
                log::debug!("could not restrict the audit log directory permissions: {e}");
            }
        }
        self.state.borrow_mut().parent_ready = true;
        Ok(())
    }

    #[cfg(unix)]
    fn tighten_permissions(&self, file: &fs::File) {
        use std::os::unix::fs::PermissionsExt;
        if self.state.borrow().file_tightened {
            return;
        }
        // Tighten logs created world-readable by older versions; never fail
        // the append because of it.
        if let Err(e) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
            log::debug!("could not restrict the audit log permissions: {e}");
        }
        self.state.borrow_mut().file_tightened = true;
    }

    /// Rotate the log once it grows past [`MAX_SIZE`].
    ///
    /// Best effort: when the rename fails (a reader holds the file on Windows,
    /// a read-only directory, ...) the current file keeps growing and the
    /// rotation is retried on the next append instead of dropping the event.
    fn rotate_if_needed(&self) {
        let size = fs::metadata(&self.path).map(|meta| meta.len()).unwrap_or(0);
        if size < MAX_SIZE {
            return;
        }
        let rotated = self.path.with_extension("jsonl.1");
        // Replace the previous rotation; losing an old audit file is better
        // than failing to record new events.
        let _ = fs::remove_file(&rotated);
        match fs::rename(&self.path, &rotated) {
            Ok(()) => self.after_rotation(&rotated),
            Err(e) => log::debug!("could not rotate the audit log, will retry: {e}"),
        }
    }

    /// Bookkeeping after a successful rotation.
    ///
    /// Kept as a separate function so the control flow does not depend on
    /// cfg(unix): with the permission block compiled out on Windows, an early
    /// return would otherwise be the last statement of rotate_if_needed and
    /// trip clippy::needless_return (CI builds with -D warnings).
    #[cfg(unix)]
    fn after_rotation(&self, rotated: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(rotated, fs::Permissions::from_mode(0o600));
        // The next append recreates the log file with the process umask, so
        // the one-shot tightening must run again for the new file; otherwise
        // every record written after the first rotation would be 0644.
        self.state.borrow_mut().file_tightened = false;
    }

    #[cfg(not(unix))]
    fn after_rotation(&self, _rotated: &std::path::Path) {}

    /// Most recent records, newest first, capped at `limit`.
    ///
    /// The whole file is read (bounded by [`MAX_SIZE`]); malformed lines are
    /// skipped so a torn write from an older version cannot break history.
    pub(crate) fn recent(&self, limit: usize) -> Vec<AuditRecord> {
        let Ok(content) = fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let mut records = content
            .lines()
            .filter_map(|line| match serde_json::from_str::<AuditRecord>(line) {
                Ok(record) => Some(record),
                Err(e) => {
                    log::debug!("ignoring malformed audit line: {e}");
                    None
                }
            })
            .collect::<Vec<_>>();
        records.reverse();
        records.truncate(limit);
        records
    }
}

fn record(
    event: &str,
    fingerprint: Option<&str>,
    name: Option<&str>,
    addr: Option<&str>,
    detail: Option<&str>,
) -> AuditRecord {
    AuditRecord {
        ts: unix_secs(),
        event: event.to_string(),
        fingerprint: fingerprint.map(str::to_string),
        name: name.map(str::to_string),
        addr: addr.map(str::to_string),
        detail: detail.map(str::to_string),
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_log() -> (AuditLog, PathBuf) {
        let dir = std::env::temp_dir().join("lan-mouse-audit-tests");
        std::fs::create_dir_all(&dir).expect("test dir");
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("audit-{}-{id}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        (AuditLog::new(path.clone()), path)
    }

    #[test]
    fn records_are_appended_as_json_lines() {
        let (audit, path) = temp_log();
        audit.event(
            "grant_created",
            Some("aa:bb"),
            Some("laptop"),
            Some("10.0.0.2:4242"),
            Some("source=manual incoming=pointer,keyboard"),
        );
        audit.event("revoke", Some("aa:bb"), None, None, Some("reason=user"));
        let content = std::fs::read_to_string(&path).expect("audit file");
        assert_eq!(content.lines().count(), 2);
        let first: AuditRecord =
            serde_json::from_str(content.lines().next().unwrap()).expect("json");
        assert_eq!(first.event, "grant_created");
        assert_eq!(first.fingerprint.as_deref(), Some("aa:bb"));
        assert_eq!(first.name.as_deref(), Some("laptop"));
        assert!(first.ts > 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn recent_returns_newest_first_and_caps_the_list() {
        let (audit, path) = temp_log();
        for index in 0..5 {
            audit.event(&format!("event_{index}"), None, None, None, None);
        }
        let recent = audit.recent(3);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].event, "event_4");
        assert_eq!(recent[2].event, "event_2");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn recent_on_a_missing_file_is_empty() {
        let (audit, path) = temp_log();
        assert!(audit.recent(10).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rotation_starts_a_fresh_file() {
        let (audit, path) = temp_log();
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        // Fake an oversized log so the next append rotates it.
        std::fs::write(&path, vec![b'x'; MAX_SIZE as usize + 1]).expect("big file");
        audit.event("after_rotation", None, None, None, None);
        assert!(path.exists(), "a new audit file is created");
        assert!(
            path.with_extension("jsonl.1").exists(),
            "the previous file is kept as .1"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("jsonl.1"));
    }

    #[cfg(unix)]
    #[test]
    fn the_file_recreated_after_a_rotation_is_private_again() {
        use std::os::unix::fs::PermissionsExt;

        let (audit, path) = temp_log();
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, vec![b'x'; MAX_SIZE as usize + 1]).expect("big file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("mode");

        audit.event("before_rotation", None, None, None, None);
        assert_eq!(
            std::fs::metadata(&path).expect("file").permissions().mode() & 0o777,
            0o600,
            "the audit log must be private"
        );

        // Force a second rotation: the recreated file must be tightened again.
        // A one-shot guard used to leave the new file at the process umask
        // (typically 0644), exposing fingerprints/addresses after rotation.
        std::fs::write(&path, vec![b'x'; MAX_SIZE as usize + 1]).expect("big file");
        audit.event("after_second_rotation", None, None, None, None);
        assert_eq!(
            std::fs::metadata(&path).expect("file").permissions().mode() & 0o777,
            0o600,
            "the file recreated after a rotation must be tightened again"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("jsonl.1"));
    }

    #[test]
    fn rotation_failure_does_not_drop_the_event() {
        let (audit, path) = temp_log();
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, vec![b'x'; MAX_SIZE as usize + 1]).expect("big file");
        // Occupy the rotation target with a directory so both remove_file and
        // rename fail; the event must still be appended to the current file.
        let rotated = path.with_extension("jsonl.1");
        std::fs::create_dir_all(&rotated).expect("blocked rotation target");
        audit.event("after_failed_rotation", None, None, None, None);
        let content = std::fs::read(&path).expect("current file");
        let needle = b"after_failed_rotation";
        assert!(
            content.windows(needle.len()).any(|window| window == needle),
            "the event is appended even when the rotation rename fails"
        );
        let _ = std::fs::remove_dir_all(&rotated);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let (audit, path) = temp_log();
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, "not json\n{\"ts\":1,\"event\":\"ok\"}\n").expect("write");
        let recent = audit.recent(10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].event, "ok");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn state_changes_are_suppressed_only_inside_the_window() {
        let (audit, path) = temp_log();
        let now = Instant::now();
        let window = Duration::from_secs(10);
        assert!(audit.allows_write("key-a", window, now));
        audit.mark_written("key-a", now);
        assert!(!audit.allows_write("key-a", window, now + Duration::from_secs(5)));
        assert!(audit.allows_write("key-b", window, now + Duration::from_secs(5)));
        assert!(audit.allows_write("key-a", window, now + Duration::from_secs(11)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn state_change_tracking_is_bounded() {
        let (audit, path) = temp_log();
        let now = Instant::now();
        for index in 0..(MAX_TRACKED_CHANGES + 10) {
            let key = format!("key-{index}");
            assert!(audit.allows_write(&key, Duration::from_secs(60), now));
            audit.mark_written(&key, now);
        }
        assert!(audit.state.borrow().last_written.len() <= MAX_TRACKED_CHANGES);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn event_state_change_writes_once_per_state() {
        let (audit, path) = temp_log();
        let window = Duration::from_secs(60);
        assert!(audit.event_state_change(
            "peer_permissions:aa:1:2",
            window,
            "peer_permissions",
            Some("aa:bb"),
            None,
            Some("10.0.0.2:5555"),
            Some("peer_incoming=0x1 peer_outgoing=0x2"),
        ));
        assert!(!audit.event_state_change(
            "peer_permissions:aa:1:2",
            window,
            "peer_permissions",
            Some("aa:bb"),
            None,
            Some("10.0.0.2:6666"),
            Some("peer_incoming=0x1 peer_outgoing=0x2"),
        ));
        // A changed state writes again.
        assert!(audit.event_state_change(
            "peer_permissions:aa:3:2",
            window,
            "peer_permissions",
            Some("aa:bb"),
            None,
            Some("10.0.0.2:6666"),
            Some("peer_incoming=0x3 peer_outgoing=0x2"),
        ));
        assert_eq!(audit.recent(10).len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn log_and_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "lan-mouse-audit-perms-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let path = dir.join("audit.jsonl");
        let audit = AuditLog::new(path.clone());
        audit.event("test", None, None, None, None);
        assert_eq!(
            std::fs::metadata(&path).expect("log").permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&dir).expect("dir").permissions().mode() & 0o777,
            0o700
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
