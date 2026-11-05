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

use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Rotate the log once it grows past this size.
pub(crate) const MAX_SIZE: u64 = 5 * 1024 * 1024;

/// One line of the audit log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AuditRecord {
    /// unix timestamp in seconds
    pub ts: u64,
    /// stable machine readable event name (`grant_created`, `revoke`, …)
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

/// Append-only writer/reader for the audit log.
#[derive(Debug, Clone)]
pub(crate) struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
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
            let state = std::env::var("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|_| {
                    std::env::var("HOME").map(|home| PathBuf::from(home).join(".local/state"))
                })
                .unwrap_or_else(|_| PathBuf::from("."));
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
        let record = AuditRecord {
            ts: unix_secs(),
            event: event.to_string(),
            fingerprint: fingerprint.map(str::to_string),
            name: name.map(str::to_string),
            addr: addr.map(str::to_string),
            detail: detail.map(str::to_string),
        };
        if let Err(e) = self.append(&record) {
            log::warn!("failed to write audit log {:?}: {e}", self.path);
        }
    }

    fn append(&self, record: &AuditRecord) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        self.rotate_if_needed()?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = serde_json::to_string(record).map_err(std::io::Error::other)?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        Ok(())
    }

    fn rotate_if_needed(&self) -> std::io::Result<()> {
        let size = fs::metadata(&self.path).map(|meta| meta.len()).unwrap_or(0);
        if size < MAX_SIZE {
            return Ok(());
        }
        let rotated = self.path.with_extension("jsonl.1");
        // Replace the previous rotation; losing an old audit file is better
        // than failing to record new events.
        let _ = fs::remove_file(&rotated);
        fs::rename(&self.path, &rotated)?;
        Ok(())
    }

    /// Most recent records, newest first, capped at `limit`.
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
}
