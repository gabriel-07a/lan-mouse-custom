//! One text file with what is needed to understand a problem on this PC:
//! versions, backends, the state and latency of every link, devices, the
//! configuration, and the end of the hand-off trace and the audit log. The
//! user generates it (GUI Settings, `lan-mouse cli diagnose`) on each PC and
//! sends the files; nothing leaves the machine by itself.
//!
//! The private key is never read. The report does contain host names, IP
//! addresses and certificate fingerprints (public identities), which the
//! header says.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

/// Lines kept from the end of each log-like file.
const TRACE_TAIL_LINES: usize = 3000;
const AUDIT_TAIL_LINES: usize = 300;

/// Everything the report is made of, gathered by the service.
pub(crate) struct ReportInput {
    pub(crate) generated_at: String,
    pub(crate) app_version: String,
    pub(crate) commit: String,
    pub(crate) os: String,
    pub(crate) host: String,
    /// sections of `(title, body)`, in order
    pub(crate) sections: Vec<(String, String)>,
    /// files whose end goes into the report: `(title, path, lines)`
    pub(crate) tails: Vec<(String, Option<PathBuf>, usize)>,
}

impl ReportInput {
    pub(crate) fn trace_tail(path: Option<PathBuf>) -> (String, Option<PathBuf>, usize) {
        ("Hand-off trace (end)".into(), path, TRACE_TAIL_LINES)
    }

    pub(crate) fn audit_tail(path: PathBuf) -> (String, Option<PathBuf>, usize) {
        ("Audit log (end)".into(), Some(path), AUDIT_TAIL_LINES)
    }
}

/// The report text.
pub(crate) fn build(input: &ReportInput) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Lan Mouse diagnostic report");
    let _ = writeln!(out, "generated: {}", input.generated_at);
    let _ = writeln!(out, "version:   {} ({})", input.app_version, input.commit);
    let _ = writeln!(out, "system:    {}", input.os);
    let _ = writeln!(out, "host:      {}", input.host);
    let _ = writeln!(
        out,
        "\nContains host names, IP addresses and certificate fingerprints (public \
         identities). It never contains the private key. Generate one on each PC \
         and send both."
    );
    for (title, body) in &input.sections {
        section(&mut out, title, body);
    }
    for (title, path, lines) in &input.tails {
        let body = match path {
            Some(path) => match tail(path, *lines) {
                Some(text) if !text.is_empty() => {
                    format!("{} (last {lines} lines)\n{text}", path.display())
                }
                Some(_) => format!("{} is empty", path.display()),
                None => format!("{} not found", path.display()),
            },
            None => "not available on this system".into(),
        };
        section(&mut out, title, &body);
    }
    out
}

fn section(out: &mut String, title: &str, body: &str) {
    let _ = writeln!(out, "\n===== {title} =====");
    let _ = writeln!(out, "{}", body.trim_end());
}

/// The last `lines` lines of `path` (`None` when it cannot be read).
fn tail(path: &Path, lines: usize) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    Some(all[start..].join("\n"))
}

/// `(human, file name)` forms of a Unix time, in UTC:
/// `2026-11-26 13:05:09 UTC` and `20261126-130509`.
pub(crate) fn utc_stamp(unix_secs: u64) -> (String, String) {
    let days = (unix_secs / 86_400) as i64;
    let secs = unix_secs % 86_400;
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        format!("{year:04}-{month:02}-{day:02} {h:02}:{m:02}:{s:02} UTC"),
        format!("{year:04}{month:02}{day:02}-{h:02}{m:02}{s:02}"),
    )
}

/// Write `report` into `dir` with a timestamped name; returns the file.
pub(crate) fn write(dir: &Path, stamp: &str, report: &str) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("lan-mouse-report-{stamp}.txt"));
    fs::write(&path, report)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(tails: Vec<(String, Option<PathBuf>, usize)>) -> ReportInput {
        ReportInput {
            generated_at: "2026-11-26 10:00:00".into(),
            app_version: "0.2.22".into(),
            commit: "abc1234".into(),
            os: "linux x86_64".into(),
            host: "predator".into(),
            sections: vec![("Links".into(), "client 0: connected\n".into())],
            tails,
        }
    }

    #[test]
    fn the_report_has_the_header_sections_and_file_tails() {
        let env = lan_mouse_test_support::env::TestEnv::new("diagnostic-report");
        let lines: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        let log = env.write("trace.jsonl", &lines);
        let report = build(&input(vec![
            ("Trace".into(), Some(log), 3),
            ("Missing".into(), Some(env.root().join("nope.log")), 3),
            ("Not here".into(), None, 3),
        ]));
        assert!(report.starts_with("Lan Mouse diagnostic report"));
        assert!(report.contains("version:   0.2.22 (abc1234)"));
        assert!(report.contains("never contains the private key"));
        assert!(report.contains("===== Links =====\nclient 0: connected"));
        assert!(report.contains("line 8\nline 9\nline 10"), "{report}");
        assert!(!report.contains("line 7"), "only the end of the file");
        assert!(report.contains("nope.log not found"));
        assert!(report.contains("===== Not here =====\nnot available on this system"));
    }

    #[test]
    fn unix_time_is_formatted_in_utc() {
        assert_eq!(
            utc_stamp(0),
            ("1970-01-01 00:00:00 UTC".into(), "19700101-000000".into())
        );
        // 2026-11-26 13:05:09 UTC
        assert_eq!(utc_stamp(1_795_698_309).1, "20261126-130509");
        // a leap day
        assert_eq!(utc_stamp(1_709_164_800).0, "2024-02-29 00:00:00 UTC");
    }

    #[test]
    fn the_report_is_written_with_a_timestamped_name() {
        let env = lan_mouse_test_support::env::TestEnv::new("diagnostic-write");
        let path = write(&env.root().join("reports"), "20261126-100000", "hello").expect("write");
        assert!(path.ends_with("reports/lan-mouse-report-20261126-100000.txt"));
        assert_eq!(fs::read_to_string(path).unwrap(), "hello");
    }
}
