//! `lan-mouse doctor` — diagnose why a device can or cannot send input.
//!
//! The command is intentionally read-only (except for creating the
//! certificate when it does not exist yet) and does not require a running
//! service. With `--probe` it also initializes the input backends, which is
//! the only reliable way to find out which backend the current session will
//! actually use.

use clap::Args;
use serde::Serialize;

use crate::config::Config;

#[derive(Args, Clone, Debug, Eq, PartialEq)]
pub struct DoctorArgs {
    /// initialize the input capture/emulation backends (may prompt for
    /// permissions and briefly create a session)
    #[arg(long)]
    probe: bool,

    /// machine readable output
    #[arg(long)]
    json: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DoctorError {
    #[error("could not load certificate: {0}")]
    Certificate(#[from] crate::crypto::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Default, Serialize)]
struct Probe {
    backend: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ClientReport {
    handle: u64,
    hostname: Option<String>,
    position: String,
    active: bool,
    port: u16,
    ips: Vec<String>,
}

/// Which portal a restore token belongs to.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
enum PortalTokenKind {
    /// `org.freedesktop.portal.InputCapture` (outgoing input capture)
    InputCapture,
    /// `org.freedesktop.portal.RemoteDesktop` (input emulation)
    InputEmulation,
    /// `org.freedesktop.portal.Clipboard` on top of RemoteDesktop
    Clipboard,
}

/// Location of one portal restore token.
///
/// Only the location and existence are reported: the token itself is a bearer
/// capability and must never end up in logs or issue reports.
#[derive(Debug, Serialize)]
struct PortalTokenReport {
    kind: PortalTokenKind,
    /// what the token authorizes
    purpose: &'static str,
    /// current `$XDG_STATE_HOME/lan-mouse` location, if resolvable
    path: Option<String>,
    /// whether a token was found at `path`
    in_state: bool,
    /// whether a usable token exists (state location or legacy cache copy)
    present: bool,
    /// old `$XDG_CACHE_HOME/lan-mouse` location, if a stale token is still there
    legacy_path: Option<String>,
}

/// Directory where portal tokens are stored: `$XDG_STATE_HOME/lan-mouse`,
/// falling back to `~/.local/state/lan-mouse` (state, not cache, so cleanup
/// tools do not purge the user's persisted consents).
#[cfg(all(unix, not(target_os = "macos")))]
fn state_token_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/state"))
        })?;
    Some(base.join("lan-mouse"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn legacy_token_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".cache"))
        })?;
    Some(base.join("lan-mouse"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn token_present(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|contents| !contents.trim().is_empty())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn portal_tokens() -> Vec<PortalTokenReport> {
    const TOKENS: [(PortalTokenKind, &str, &str); 3] = [
        (
            PortalTokenKind::InputCapture,
            "input capture (InputCapture portal)",
            "input-capture.token",
        ),
        (
            PortalTokenKind::InputEmulation,
            "input emulation (RemoteDesktop portal)",
            "remote-desktop.token",
        ),
        (
            PortalTokenKind::Clipboard,
            "clipboard (RemoteDesktop/Clipboard portal)",
            "clipboard.token",
        ),
    ];

    let state_dir = state_token_dir();
    let legacy_dir = legacy_token_dir();
    TOKENS
        .iter()
        .map(|&(kind, purpose, file)| {
            let path = state_dir.as_ref().map(|dir| dir.join(file));
            let present_in_state = path.as_deref().is_some_and(token_present);
            let legacy_path = legacy_dir
                .as_ref()
                .map(|dir| dir.join(file))
                .filter(|path| token_present(path));
            PortalTokenReport {
                kind,
                purpose,
                // A token left in the legacy cache directory is still usable:
                // the runtime migrates it on the next read.
                present: present_in_state || legacy_path.is_some(),
                in_state: present_in_state,
                path: path.map(|path| path.display().to_string()),
                legacy_path: legacy_path.map(|path| path.display().to_string()),
            }
        })
        .collect()
}

#[derive(Debug, Serialize)]
struct Report {
    version: String,
    commit: String,
    config_path: String,
    service_running: bool,
    session_type: Option<String>,
    desktop: Option<String>,
    port: u16,
    capture_backend_requested: Option<String>,
    emulation_backend_requested: Option<String>,
    capture_probe: Option<Probe>,
    emulation_probe: Option<Probe>,
    clipboard_portal: Option<String>,
    clipboard_consent: bool,
    /// directory holding the portal restore tokens (state, not cache)
    portal_token_dir: Option<String>,
    /// portal restore tokens found on disk, by portal
    portal_tokens: Vec<PortalTokenReport>,
    local_fingerprint: String,
    authorized_fingerprints: Vec<String>,
    /// where the append-only audit log lives
    audit_log: String,
    /// number of audit entries read as a cheap health signal
    recent_audit_events: usize,
    clients: Vec<ClientReport>,
    warnings: Vec<String>,
}

/// Warnings about missing or stale portal consent tokens.
///
/// `linux_portal_session` gates the InputCapture hint: on plain X11 sessions
/// the libei capture portal is not used, so a missing capture token is normal.
fn portal_token_warnings(
    tokens: &[PortalTokenReport],
    linux_portal_session: bool,
    clipboard_portal_available: bool,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for token in tokens {
        if let Some(legacy_path) = &token.legacy_path {
            warnings.push(format!(
                "restore token for {} still lives in the legacy cache directory \
                 ({legacy_path}); it is migrated automatically on the next start",
                token.purpose
            ));
        }
        if token.present {
            continue;
        }
        match token.kind {
            PortalTokenKind::InputCapture => {
                if linux_portal_session {
                    warnings.push(
                        "no input-capture consent token: the compositor shows its capture \
                         permission dialog whenever a capture session starts; the GNOME \
                         InputCapture portal cannot persist this consent (see \
                         docs/audit-ubuntu-permissions.md)"
                            .into(),
                    );
                }
            }
            PortalTokenKind::InputEmulation => warnings.push(
                "no input-emulation consent token: the RemoteDesktop dialog will appear on \
                 first use; tick \"Remember This Selection\" so it is only asked once"
                    .into(),
            ),
            PortalTokenKind::Clipboard => {
                if clipboard_portal_available {
                    warnings.push(
                        "no clipboard consent token: run lan-mouse, enable clipboard sync and \
                         allow \"Allow Clipboard Access\" (tick \"Remember This Selection\")"
                            .into(),
                    );
                }
            }
        }
    }
    warnings
}

fn build_warnings(clients: &[ClientReport], authorized_fingerprints: usize) -> Vec<String> {
    let mut warnings = Vec::new();
    if clients.is_empty() {
        warnings.push(
            "no clients configured: this machine can only receive input, never send it".to_string(),
        );
    }
    if !clients.iter().any(|c| c.active) {
        warnings.push(
            "no active client: outgoing input is disabled until a client is activated".to_string(),
        );
    }
    let mut seen_positions: std::collections::HashMap<&str, usize> = Default::default();
    for client in clients.iter().filter(|c| c.active) {
        *seen_positions.entry(client.position.as_str()).or_default() += 1;
    }
    for (position, count) in seen_positions {
        if count > 1 {
            warnings.push(format!(
                "{count} active clients share the position `{position}`; only one of them can \
                 capture at a time"
            ));
        }
    }
    for client in clients.iter() {
        if client.hostname.is_none() && client.ips.is_empty() {
            warnings.push(format!(
                "client {} (pos {}) has neither a hostname nor an IP address",
                client.handle, client.position
            ));
        }
    }
    if authorized_fingerprints == 0 {
        warnings.push(
            "no authorized fingerprints: incoming connections will be rejected until the peer is \
             authorized"
                .to_string(),
        );
    }
    warnings
}

pub async fn run(config: Config, args: DoctorArgs) -> Result<(), DoctorError> {
    let cert = crate::crypto::load_or_generate_key_and_cert(config.cert_path())?;
    let local_fingerprint = crate::crypto::certificate_fingerprint(&cert);

    let authorized = config.authorized_fingerprints();
    let mut authorized_fingerprints = authorized
        .iter()
        .map(|(fingerprint, description)| format!("{fingerprint} ({description})"))
        .collect::<Vec<_>>();
    authorized_fingerprints.sort();

    let clients = config
        .clients()
        .into_iter()
        .enumerate()
        .map(|(handle, client)| ClientReport {
            handle: handle as u64,
            hostname: client.hostname,
            position: client.pos.to_string(),
            active: client.active,
            port: client.port,
            ips: client.ips.into_iter().map(|ip| ip.to_string()).collect(),
        })
        .collect::<Vec<_>>();

    let mut warnings = build_warnings(&clients, authorized_fingerprints.len());

    #[cfg(all(unix, not(target_os = "macos")))]
    let clipboard_portal = input_clipboard::portal_available_version()
        .await
        .ok()
        .map(|version| format!("v{version}"));
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    let clipboard_portal: Option<String> = None;
    #[cfg(all(unix, not(target_os = "macos")))]
    let clipboard_consent = input_clipboard::portal_restore_token_present();
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    let clipboard_consent = false;

    #[cfg(all(unix, not(target_os = "macos")))]
    let portal_tokens = portal_tokens();
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    let portal_tokens: Vec<PortalTokenReport> = Vec::new();
    #[cfg(all(unix, not(target_os = "macos")))]
    let portal_token_dir = state_token_dir().map(|dir| dir.display().to_string());
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    let portal_token_dir: Option<String> = None;
    // The libei capture backend (and therefore its consent token) only exists
    // on Wayland portal sessions.
    let linux_portal_session = std::env::var("XDG_SESSION_TYPE")
        .is_ok_and(|session| session == "wayland")
        || std::env::var_os("WAYLAND_DISPLAY").is_some();
    warnings.extend(portal_token_warnings(
        &portal_tokens,
        linux_portal_session,
        clipboard_portal.is_some(),
    ));

    let mut capture_probe = None;
    let mut emulation_probe = None;
    if args.probe {
        capture_probe = Some(probe_capture(&config).await);
        emulation_probe = Some(probe_emulation(&config).await);
    }

    let audit = crate::audit::AuditLog::new(crate::audit::AuditLog::default_path());
    let audit_log = audit.path().display().to_string();
    let recent_audit_events = audit.recent(20).len();

    let report = Report {
        version: env!("CARGO_PKG_VERSION").to_string(),
        commit: String::from_utf8_lossy(&crate::config::local_commit()).to_string(),
        config_path: config.config_path().display().to_string(),
        service_running: lan_mouse_ipc::is_service_running(),
        session_type: std::env::var("XDG_SESSION_TYPE").ok(),
        desktop: std::env::var("XDG_CURRENT_DESKTOP").ok(),
        port: config.port(),
        capture_backend_requested: config.capture_backend().map(|backend| backend.to_string()),
        emulation_backend_requested: config
            .emulation_backend()
            .map(|backend| backend.to_string()),
        capture_probe,
        emulation_probe,
        clipboard_portal,
        clipboard_consent,
        portal_token_dir,
        portal_tokens,
        local_fingerprint,
        authorized_fingerprints,
        audit_log,
        recent_audit_events,
        clients,
        warnings,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("lan-mouse doctor");
    println!("  version:         {}", report.version);
    println!("  commit:          {}", report.commit);
    println!("  config:          {}", report.config_path);
    println!(
        "  audit log:       {} ({} recent event(s))",
        report.audit_log, report.recent_audit_events
    );
    println!(
        "  service running: {}",
        if report.service_running { "yes" } else { "no" }
    );
    println!(
        "  session:         {} / {}",
        report.session_type.as_deref().unwrap_or("unknown"),
        report.desktop.as_deref().unwrap_or("unknown")
    );
    println!("  port:            {}", report.port);
    println!(
        "  capture backend: {}",
        report
            .capture_backend_requested
            .as_deref()
            .unwrap_or("auto")
    );
    println!(
        "  emulation:       {}",
        report
            .emulation_backend_requested
            .as_deref()
            .unwrap_or("auto")
    );
    if let Some(probe) = &report.capture_probe {
        println!("  capture probe:   {}", format_probe(probe));
    }
    if let Some(probe) = &report.emulation_probe {
        println!("  emulation probe: {}", format_probe(probe));
    }
    println!(
        "  clipboard portal: {}{}",
        report.clipboard_portal.as_deref().unwrap_or("unavailable"),
        if report.clipboard_consent {
            " (consent saved)"
        } else if report.clipboard_portal.is_some() {
            " (consent missing)"
        } else {
            ""
        }
    );
    if !report.portal_tokens.is_empty() {
        println!(
            "  portal tokens:   {}",
            report.portal_token_dir.as_deref().unwrap_or("unknown")
        );
        for token in &report.portal_tokens {
            match (&token.path, token.in_state, token.present) {
                (Some(path), true, _) => println!("    {}: present ({path})", token.purpose),
                (Some(path), false, true) => println!(
                    "    {}: present in legacy cache ({})",
                    token.purpose,
                    token.legacy_path.as_deref().unwrap_or(path)
                ),
                (Some(path), false, false) => println!("    {}: missing ({path})", token.purpose),
                (None, _, _) => println!("    {}: unknown (HOME is not set)", token.purpose),
            }
            if token.in_state {
                if let Some(legacy) = &token.legacy_path {
                    println!("      legacy copy: {legacy} (migrated on next start)");
                }
            }
        }
    }
    println!("  fingerprint:     {}", report.local_fingerprint);
    println!(
        "  authorized:      {}",
        if report.authorized_fingerprints.is_empty() {
            "none".to_string()
        } else {
            report.authorized_fingerprints.join(", ")
        }
    );

    println!("  clients:");
    if report.clients.is_empty() {
        println!("    (none)");
    }
    for client in &report.clients {
        println!(
            "    [{}] {} pos={} active={} port={} ips=[{}]",
            client.handle,
            client.hostname.as_deref().unwrap_or("<no hostname>"),
            client.position,
            client.active,
            client.port,
            client.ips.join(", ")
        );
    }

    if report.warnings.is_empty() {
        println!("  warnings:        none");
    } else {
        println!("  warnings:");
        for warning in &report.warnings {
            println!("    - {warning}");
        }
    }

    Ok(())
}

fn format_probe(probe: &Probe) -> String {
    match (&probe.backend, &probe.error) {
        (Some(backend), _) => backend.clone(),
        (None, Some(error)) => format!("unavailable ({error})"),
        (None, None) => "unknown".to_string(),
    }
}

async fn probe_capture(config: &Config) -> Probe {
    match input_capture::InputCapture::new(config.capture_backend().map(Into::into)).await {
        Ok(mut capture) => {
            let backend = Some(capture.backend().to_string());
            if let Err(e) = capture.terminate().await {
                log::debug!("failed to terminate capture after probe: {e}");
            }
            Probe {
                backend,
                error: None,
            }
        }
        Err(e) => Probe {
            backend: None,
            error: Some(e.to_string()),
        },
    }
}

async fn probe_emulation(config: &Config) -> Probe {
    match input_emulation::InputEmulation::new(config.emulation_backend().map(Into::into)).await {
        Ok(mut emulation) => {
            let backend = Some(emulation.backend().to_string());
            emulation.terminate().await;
            Probe {
                backend,
                error: None,
            }
        }
        Err(e) => Probe {
            backend: None,
            error: Some(e.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(handle: u64, position: &str, active: bool) -> ClientReport {
        ClientReport {
            handle,
            hostname: Some(format!("pc-{handle}")),
            position: position.to_string(),
            active,
            port: 4242,
            ips: vec!["192.168.1.10".to_string()],
        }
    }

    #[test]
    fn empty_setup_warns_about_every_aspect() {
        let warnings = build_warnings(&[], 0);
        assert!(warnings.iter().any(|w| w.contains("no clients configured")));
        assert!(warnings.iter().any(|w| w.contains("no active client")));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("no authorized fingerprints"))
        );
    }

    #[test]
    fn a_complete_setup_has_no_warnings() {
        let warnings = build_warnings(&[client(0, "left", true)], 1);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn duplicate_active_positions_are_reported() {
        let warnings = build_warnings(&[client(0, "left", true), client(1, "left", true)], 1);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("share the position `left`"));
    }

    #[test]
    fn inactive_clients_do_not_conflict() {
        let warnings = build_warnings(&[client(0, "left", true), client(1, "left", false)], 1);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn client_without_hostname_or_ip_is_reported() {
        let mut broken = client(3, "top", true);
        broken.hostname = None;
        broken.ips.clear();
        let warnings = build_warnings(&[broken], 1);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("client 3"));
        assert!(warnings[0].contains("neither a hostname nor an IP"));
    }

    #[test]
    fn probe_formatting_covers_every_state() {
        assert_eq!(
            format_probe(&Probe {
                backend: Some("libei".into()),
                error: None,
            }),
            "libei"
        );
        assert_eq!(
            format_probe(&Probe {
                backend: None,
                error: Some("no session".into()),
            }),
            "unavailable (no session)"
        );
        assert_eq!(format_probe(&Probe::default()), "unknown");
    }

    fn portal_token(
        kind: PortalTokenKind,
        present: bool,
        legacy: Option<&str>,
    ) -> PortalTokenReport {
        PortalTokenReport {
            kind,
            purpose: "test portal",
            path: Some("/state/lan-mouse/test.token".to_string()),
            in_state: present,
            present: present || legacy.is_some(),
            legacy_path: legacy.map(str::to_string),
        }
    }

    #[test]
    fn missing_portal_tokens_are_reported_for_each_portal() {
        let tokens = [
            portal_token(PortalTokenKind::InputCapture, false, None),
            portal_token(PortalTokenKind::InputEmulation, false, None),
            portal_token(PortalTokenKind::Clipboard, false, None),
        ];
        let warnings = portal_token_warnings(&tokens, true, true);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("input-capture")));
        assert!(warnings.iter().any(|w| w.contains("input-emulation")));
        assert!(warnings.iter().any(|w| w.contains("clipboard")));
    }

    #[test]
    fn present_tokens_do_not_warn() {
        let tokens = [
            portal_token(PortalTokenKind::InputCapture, true, None),
            portal_token(PortalTokenKind::InputEmulation, true, None),
            portal_token(PortalTokenKind::Clipboard, true, None),
        ];
        assert!(portal_token_warnings(&tokens, true, true).is_empty());
    }

    #[test]
    fn capture_warning_is_only_emitted_on_portal_sessions() {
        let tokens = [portal_token(PortalTokenKind::InputCapture, false, None)];
        assert!(portal_token_warnings(&tokens, false, false).is_empty());
        assert_eq!(portal_token_warnings(&tokens, true, false).len(), 1);
    }

    #[test]
    fn stale_legacy_tokens_are_flagged_for_migration() {
        let tokens = [portal_token(
            PortalTokenKind::InputEmulation,
            true,
            Some("/cache/lan-mouse/remote-desktop.token"),
        )];
        let warnings = portal_token_warnings(&tokens, true, true);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("legacy cache directory"));
        assert!(warnings[0].contains("/cache/lan-mouse/remote-desktop.token"));
    }
}
