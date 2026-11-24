//! Opt-in structured tracing for the mouse hand-off.
//!
//! `LAN_MOUSE_HANDOFF_TRACE=1` writes JSON Lines to a file under the user's
//! cache directory (override with `LAN_MOUSE_HANDOFF_TRACE_FILE`). One line per
//! event keeps the journal readable while still giving a machine-parseable
//! trace for latency/stutter analysis. The file rotates at 5 MiB (one `.1`
//! backup).
//!
//! Schema (stable enough for scripts):
//!
//! ```json
//! {"ts_unix_us":1700000000000000,"role":"receiver","stage":"receiver.armed",
//!  "serial":109,"peer":"192.168.1.42:52706","elapsed_us":0,"along":0.37}
//! ```
//!
//! `role` is `controller` (cursor leaves this machine) or `receiver` (input is
//! injected here). `serial` is the `Enter` serial when known. Additional
//! fields are event-specific (`elapsed_us`, `gap_us`, `dx`, `dy`, ...).

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// A gap between two injected inputs above this value is worth a trace line.
pub(crate) const INPUT_GAP_WARN: Duration = Duration::from_millis(8);

const MAX_TRACE_BYTES: u64 = 5 * 1024 * 1024;

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var("LAN_MOUSE_HANDOFF_TRACE").is_ok_and(|value| value == "1"))
}

fn trace_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("LAN_MOUSE_HANDOFF_TRACE_FILE") {
        return Some(PathBuf::from(path));
    }
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));
    base.map(|base| base.join("lan-mouse").join("handoff-trace.jsonl"))
}

/// Last `Enter` serial seen from each receiver peer, so input/gap events can
/// carry the same serial even though the emulation layer does not receive it.
fn receiver_serials() -> &'static Mutex<HashMap<String, u32>> {
    static SERIALS: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    SERIALS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Remember the serial of the current incoming hand-off.
pub(crate) fn set_receiver_serial(peer: &str, serial: u32) {
    if !enabled() {
        return;
    }
    let mut serials = receiver_serials()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if serials.len() > 1024 {
        serials.clear();
    }
    serials.insert(peer.to_string(), serial);
}

/// Serial associated with the current receiver session, if already seen.
pub(crate) fn receiver_serial(peer: &str) -> Option<u32> {
    receiver_serials()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(peer)
        .copied()
}

fn writer() -> &'static Mutex<Option<BufWriter<File>>> {
    static WRITER: OnceLock<Mutex<Option<BufWriter<File>>>> = OnceLock::new();
    WRITER.get_or_init(|| Mutex::new(None))
}

fn open_writer() -> Option<BufWriter<File>> {
    let path = trace_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok()?;
    }
    rotate_if_full(&path, MAX_TRACE_BYTES);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    Some(BufWriter::new(file))
}

/// Move a trace that reached `max_bytes` to `<name>.jsonl.1`, replacing the
/// previous backup: at most two files, the newest records never lost.
fn rotate_if_full(path: &std::path::Path, max_bytes: u64) {
    if fs::metadata(path).is_ok_and(|metadata| metadata.len() >= max_bytes) {
        let backup = path.with_extension("jsonl.1");
        let _ = fs::rename(path, backup);
    }
}

fn write_record(record: &serde_json::Value) {
    let mut guard = writer().lock().unwrap_or_else(|error| error.into_inner());
    if guard.is_none() {
        *guard = open_writer();
    }
    if let Some(writer) = guard.as_mut() {
        if serde_json::to_writer(&mut *writer, record).is_ok() {
            let _ = writer.write_all(b"\n");
            let _ = writer.flush();
        }
    }
}

/// Build one trace record. Kept pure for tests.
fn build_record(
    role: &str,
    stage: &str,
    serial: Option<u32>,
    peer: Option<&str>,
    fields: &[(&str, String)],
) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    object.insert("ts_unix_us".to_string(), now_unix_us().into());
    object.insert("role".to_string(), role.into());
    object.insert("stage".to_string(), stage.into());
    if let Some(serial) = serial {
        object.insert("serial".to_string(), serial.into());
    }
    if let Some(peer) = peer {
        object.insert("peer".to_string(), peer.into());
    }
    for (key, value) in fields {
        object.insert((*key).to_string(), value.clone().into());
    }
    serde_json::Value::Object(object)
}

fn now_unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

/// Record one hand-off event. No-op unless tracing is enabled.
pub(crate) fn event(
    role: &str,
    stage: &str,
    serial: Option<u32>,
    peer: Option<&str>,
    fields: &[(&str, String)],
) {
    if !enabled() {
        return;
    }
    let record = build_record(role, stage, serial, peer, fields);
    write_record(&record);
    // Keep a compact line in the journal too; the JSON file is the detailed
    // source for scripts.
    log::info!(
        target: "handoff_trace",
        "{} {} serial={:?} peer={:?}",
        role,
        stage,
        serial,
        peer
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_gap_threshold_is_eight_milliseconds() {
        assert!(!INPUT_GAP_WARN.is_zero());
        assert_eq!(INPUT_GAP_WARN, Duration::from_millis(8));
    }

    #[test]
    fn record_exposes_the_stable_schema_fields() {
        let record = build_record(
            "receiver",
            "receiver.armed",
            Some(109),
            Some("192.168.1.42:52706"),
            &[("along", "0.37".to_string())],
        );
        assert_eq!(record["role"], "receiver");
        assert_eq!(record["stage"], "receiver.armed");
        assert_eq!(record["serial"], 109);
        assert_eq!(record["peer"], "192.168.1.42:52706");
        assert_eq!(record["along"], "0.37");
        assert!(record["ts_unix_us"].as_u64().is_some());
    }

    #[test]
    fn event_is_a_noop_when_tracing_is_off() {
        if !enabled() {
            event("controller", "test", None, None, &[]);
        }
    }

    fn temp(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lan-mouse-trace-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_full_trace_becomes_the_single_backup() {
        let dir = temp("rotate");
        let path = dir.join("handoff-trace.jsonl");
        let backup = dir.join("handoff-trace.jsonl.1");
        fs::write(&backup, b"oldest").unwrap();
        fs::write(&path, vec![b'x'; 100]).unwrap();

        rotate_if_full(&path, 1000);
        assert!(path.exists(), "below the limit nothing moves");
        assert_eq!(fs::read(&backup).unwrap(), b"oldest");

        rotate_if_full(&path, 100);
        assert!(!path.exists(), "the full trace moved away");
        assert_eq!(
            fs::read(&backup).unwrap().len(),
            100,
            "it replaced the old backup"
        );
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "never more than two files"
        );

        // nothing to rotate: no error, nothing created
        rotate_if_full(&path, 100);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extra_fields_and_missing_ones_shape_the_record() {
        let record = build_record("controller", "capture.begin", None, None, &[]);
        let object = record.as_object().unwrap();
        assert!(!object.contains_key("serial") && !object.contains_key("peer"));
        assert!(object["ts_unix_us"].as_u64().unwrap() > 1_600_000_000_000_000);
        let record = build_record(
            "receiver",
            "receiver.input_gap_exceeded",
            Some(3),
            Some("10.0.0.2:1"),
            &[("gap_us", "9000".into()), ("dx", "-2.5".into())],
        );
        assert_eq!(record["gap_us"], "9000");
        assert_eq!(
            record["dx"], "-2.5",
            "values are strings, as the schema says"
        );
        // one JSON object per line: no newline can sneak into a record
        assert!(!serde_json::to_string(&record).unwrap().contains('\n'));
    }
}
