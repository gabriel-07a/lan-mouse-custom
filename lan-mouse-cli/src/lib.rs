use clap::{Args, Parser, Subcommand, ValueEnum};

use std::{
    net::IpAddr,
    path::PathBuf,
    time::{Duration, Instant},
};
use thiserror::Error;

use lan_mouse_ipc::{
    AsyncFrontendEventReader, AsyncFrontendRequestWriter, Capabilities, ClientHandle,
    ConnectionError, DirectionPolicy, FrontendEvent, FrontendRequest, IpcError, Position,
    connect_async,
};

/// How long any single request/response exchange with the service may take
/// before the CLI reports a timeout instead of hanging forever.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for an asynchronous error after a request that the IPC
/// protocol does not acknowledge explicitly (see [`settle`]).
const SETTLE_TIMEOUT: Duration = Duration::from_millis(200);

/// Accepted range for the per-client motion sensitivity, in percent. Matches
/// the clamp applied by the service (`src/service.rs`, `update_sensitivity`).
const SENSITIVITY_RANGE: std::ops::RangeInclusive<u16> = 10..=500;

/// How long `shutdown` waits for the service to close its IPC socket.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum CliError {
    /// is the service running?
    #[error("could not connect: `{0}` - is the service running?")]
    ServiceNotRunning(#[from] ConnectionError),
    #[error("error communicating with service: {0}")]
    Ipc(#[from] IpcError),
    #[error("{0}")]
    InvalidArgument(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("the service did not answer within {0:?} - is it hung?")]
    ResponseTimeout(Duration),
    #[error("the service reported an error: {0}")]
    ServiceError(String),
    #[error("no client with id {0} (use `lan-mouse-cli list` to see the configured clients)")]
    NoSuchClient(ClientHandle),
}

#[derive(Parser, Clone, Debug, PartialEq, Eq)]
#[command(name = "lan-mouse-cli", about = "LanMouse CLI interface")]
pub struct CliArgs {
    #[command(subcommand)]
    command: CliSubcommand,
}

#[derive(Args, Clone, Debug, PartialEq, Eq)]
struct Client {
    #[arg(long)]
    hostname: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    ips: Option<Vec<IpAddr>>,
    #[arg(long)]
    position: Option<Position>,
    #[arg(long)]
    activate: bool,
    #[arg(long)]
    enter_hook: Option<String>,
    #[arg(long)]
    leave_hook: Option<String>,
    #[arg(long)]
    sensitivity: Option<u16>,
    /// pinned SHA-256 fingerprint of the peer certificate
    #[arg(long)]
    fingerprint: Option<String>,
}

#[derive(Clone, Subcommand, Debug, PartialEq, Eq)]
enum CliSubcommand {
    /// add a new client
    AddClient(Client),
    /// remove an existing client
    RemoveClient { id: ClientHandle },
    /// activate a client
    Activate { id: ClientHandle },
    /// deactivate a client
    Deactivate { id: ClientHandle },
    /// list configured clients
    List,
    /// scan the local network for hosts and Lan Mouse instances
    Scan,
    /// allow connections from an address (IP or CIDR) without a prompt,
    /// with explicit permissions for the adopted devices
    Trust {
        /// IP or CIDR, e.g. `192.168.1.50` or `10.0.0.0/24`
        address: String,
        /// permissions adopted devices get here (pointer,keyboard,clipboard | all | none)
        #[arg(long, default_value = "all")]
        incoming: String,
        /// permissions this machine gets on adopted devices
        #[arg(long, default_value = "all")]
        outgoing: String,
    },
    /// list the address rules and their permissions
    Trusted,
    /// list recent audit events (grants, revokes, pairing, certificate pinning)
    History {
        #[arg(long, default_value = "50")]
        limit: u32,
    },
    /// remove an address from the allowlist
    Untrust { address: String },
    /// accept one device in the next seconds, granting the given permissions
    Pair {
        #[arg(long, default_value = "60")]
        seconds: u64,
        /// permissions the paired device gets here
        #[arg(long, default_value = "all")]
        incoming: String,
        /// permissions this machine gets on the paired device
        #[arg(long, default_value = "all")]
        outgoing: String,
    },
    /// change hostname
    SetHost {
        id: ClientHandle,
        host: Option<String>,
    },
    /// change port
    SetPort { id: ClientHandle, port: u16 },
    /// set position
    SetPosition { id: ClientHandle, pos: Position },
    /// set the motion scale of a client in percent (100 = unchanged)
    SetSensitivity { id: ClientHandle, percent: u16 },
    /// set the sensitivity of input received from peers through the absolute
    /// pointer, in percent (100 = 1:1 after monitor-scale normalisation)
    SetAbsoluteSensitivity { percent: u16 },
    /// set ips
    SetIps { id: ClientHandle, ips: Vec<IpAddr> },
    /// list known devices with their permissions
    Devices,
    /// list devices waiting for authorization
    Pending,
    /// authorize a device (or an existing pending request) with permissions
    Allow {
        /// SHA-256 fingerprint of the device
        fingerprint: String,
        /// display name for the device
        #[arg(long)]
        name: Option<String>,
        /// where the device sits relative to this one
        #[arg(long, default_value = "left")]
        position: Position,
        /// what the device may do here (pointer,keyboard,clipboard | none | all)
        #[arg(long, default_value = "all")]
        incoming: String,
        /// what this machine may do there (pointer,keyboard,clipboard | none | all)
        #[arg(long, default_value = "all")]
        outgoing: String,
        /// grant only for the current session; dropped when the peer disconnects
        #[arg(long)]
        once: bool,
    },
    /// deny a device (permanently by default, or for a limited time)
    Deny {
        /// SHA-256 fingerprint of the device
        fingerprint: String,
        /// duration until the deny expires (`1h`, `30m`, `10s`) or `forever`
        #[arg(long, default_value = "forever")]
        r#for: String,
    },
    /// revoke a device: closes live sessions and denies the fingerprint
    Revoke {
        /// SHA-256 fingerprint of the device
        fingerprint: String,
    },
    /// rename a known device
    Rename { fingerprint: String, name: String },
    /// export the permission policy (devices, address rules, denials) as JSON
    ExportPolicy { file: PathBuf },
    /// import a permission policy from a JSON file
    ImportPolicy { file: PathBuf },
    /// pin the expected certificate fingerprint of a client
    SetFingerprint {
        id: ClientHandle,
        /// SHA-256 fingerprint (`aa:bb:…`, 32 bytes)
        fingerprint: Option<String>,
    },
    /// re-enable capture
    EnableCapture,
    /// re-enable emulation
    EnableEmulation,
    /// authorize a public key
    AuthorizeKey {
        description: String,
        sha256_fingerprint: String,
    },
    /// authorize a device and configure the reverse direction automatically
    AuthorizeDevice {
        sha256_fingerprint: String,
        /// name for the new client (defaults to the peer's address)
        #[arg(long)]
        name: Option<String>,
        /// where the peer sits relative to this device
        #[arg(long, default_value = "left")]
        position: Position,
    },
    /// deauthorize a public key
    RemoveAuthorizedKey { sha256_fingerprint: String },
    /// save configuration to file
    SaveConfig,
    /// enable or disable clipboard synchronization
    Clipboard {
        #[arg(value_enum)]
        action: ClipboardAction,
    },
    /// ask the running service to shut down gracefully
    ///
    /// The service releases held keys/buttons, terminates capture, emulation,
    /// clipboard and DNS, writes its configuration and removes the IPC socket
    /// before exiting. Exits non-zero if the service does not go away in time.
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ClipboardAction {
    Enable,
    Disable,
    /// print the current state (backend, counters, last error)
    Status,
}

pub async fn run(args: CliArgs) -> Result<(), CliError> {
    execute(args.command).await?;
    Ok(())
}

/// Parse a deny duration (`forever`, `10s`, `30m`, `2h`, `1d`).
fn parse_duration(spec: &str) -> Result<Option<u64>, CliError> {
    let spec = spec.trim().to_ascii_lowercase();
    if matches!(spec.as_str(), "forever" | "permanent" | "none" | "") {
        return Ok(None);
    }
    let (number, unit) = spec.split_at(spec.len().saturating_sub(1));
    let value: u64 = number.parse().map_err(|_| {
        CliError::InvalidArgument(format!(
            "invalid duration: {spec} (use 30m, 2h, 1d or forever)"
        ))
    })?;
    let unit_seconds: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => {
            return Err(CliError::InvalidArgument(format!(
                "invalid duration: {spec} (use 30m, 2h, 1d or forever)"
            )));
        }
    };
    // Checked multiplication: "18446744073709551615d" must be rejected, not
    // panic (debug) or wrap into a tiny/negative-ish duration (release).
    let seconds = value
        .checked_mul(unit_seconds)
        .ok_or_else(|| CliError::InvalidArgument(format!("duration too large: {spec}")))?;
    Ok(Some(seconds.max(1)))
}

/// Parse `pointer,keyboard,clipboard`, `all` or `none` into a policy.
fn parse_policy(spec: &str) -> Result<DirectionPolicy, CliError> {
    match Capabilities::from_names(spec) {
        Some(capabilities) => Ok(DirectionPolicy {
            allowed: !capabilities.is_empty(),
            capabilities,
        }),
        None => Err(CliError::InvalidArgument(format!(
            "invalid capability list: {spec} (use pointer,keyboard,clipboard, all or none)"
        ))),
    }
}

/// Validate a listen/peer port. Port 0 is never a usable peer port and the
/// service would silently store it.
fn validate_port(port: u16) -> Result<u16, CliError> {
    if port == 0 {
        Err(CliError::InvalidArgument(
            "port 0 is not a valid port (use 1-65535)".to_string(),
        ))
    } else {
        Ok(port)
    }
}

/// Keep only IPv4 addresses: Lan Mouse is IPv4-only by product decision, so an
/// IPv6 `--ips` value would be silently ignored by the service.
fn validate_ips(ips: Vec<IpAddr>) -> Result<Vec<IpAddr>, CliError> {
    let (v4, ignored): (Vec<_>, Vec<_>) = ips.into_iter().partition(|ip| ip.is_ipv4());
    for ip in &ignored {
        eprintln!("warning: ignoring IPv6 address {ip} (Lan Mouse uses IPv4 only)");
    }
    if v4.is_empty() && !ignored.is_empty() {
        return Err(CliError::InvalidArgument(
            "no IPv4 address left after filtering (Lan Mouse uses IPv4 only)".to_string(),
        ));
    }
    Ok(v4)
}

/// Validate the motion sensitivity against the range the service enforces.
fn validate_sensitivity(percent: u16) -> Result<u16, CliError> {
    if SENSITIVITY_RANGE.contains(&percent) {
        Ok(percent)
    } else {
        Err(CliError::InvalidArgument(format!(
            "sensitivity {percent} is out of range ({}..={}%)",
            SENSITIVITY_RANGE.start(),
            SENSITIVITY_RANGE.end()
        )))
    }
}

/// Read the next event, turning an asynchronous `Error` reply into a CLI
/// error and enforcing a hard timeout.
///
/// `NoSuchClient` is not fatal here: it can be an unrelated state broadcast
/// (a stale handle being updated) and command-specific call sites check it
/// explicitly (`ensure_client_exists` / `settle`).
async fn next_event(rx: &mut AsyncFrontendEventReader) -> Result<FrontendEvent, CliError> {
    match rx.next_event(Some(RESPONSE_TIMEOUT)).await? {
        Some(FrontendEvent::Error(message)) => Err(CliError::ServiceError(message)),
        Some(event) => Ok(event),
        None => Err(CliError::ResponseTimeout(RESPONSE_TIMEOUT)),
    }
}

/// Fail before a command that targets a client handle when that handle does
/// not exist.
///
/// Most `Activate`/`Deactivate`/`Delete` requests for an unknown handle are
/// silently ignored by the service (no reply at all), which used to make the
/// CLI exit 0 without doing anything. Enumerating first gives a truthful
/// error; the small race with a concurrent delete is acceptable for a CLI.
async fn ensure_client_exists(
    rx: &mut AsyncFrontendEventReader,
    tx: &mut AsyncFrontendRequestWriter,
    id: ClientHandle,
) -> Result<(), CliError> {
    tx.request(FrontendRequest::Enumerate()).await?;
    loop {
        match next_event(rx).await? {
            FrontendEvent::Enumerate(clients) => {
                if clients.iter().any(|(handle, _, _)| *handle == id) {
                    return Ok(());
                }
                return Err(CliError::NoSuchClient(id));
            }
            _ => continue,
        }
    }
}

/// Wait briefly for a service-side rejection after a request that has no
/// explicit acknowledgement.
///
/// The IPC protocol replies with `Error`/`NoSuchClient` when a request is
/// rejected, and with `State`/`Deleted` for handle updates. Consuming those
/// before exiting makes the CLI exit non-zero on failure instead of printing
/// success optimistically (e.g. `allow` with an invalid fingerprint).
async fn settle(
    rx: &mut AsyncFrontendEventReader,
    handle: Option<ClientHandle>,
) -> Result<(), CliError> {
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        match rx.next_event(Some(remaining)).await? {
            None => return Ok(()),
            Some(FrontendEvent::Error(message)) => return Err(CliError::ServiceError(message)),
            Some(FrontendEvent::NoSuchClient(id)) => return Err(CliError::NoSuchClient(id)),
            Some(FrontendEvent::State(id, ..))
            | Some(FrontendEvent::Deleted(id))
            | Some(FrontendEvent::Created(id, ..))
                if handle == Some(id) =>
            {
                return Ok(());
            }
            Some(_) => continue,
        }
    }
}

/// Wait out the settle window for a rejection of an update batch.
///
/// Unlike the single-request settle, the first State event does not end the
/// wait: a batch sends several updates and only the last one may be rejected
/// (for example an invalid pinned fingerprint), so the CLI must keep reading
/// until the window expires and surface the first Error/NoSuchClient.
async fn settle_updates(
    rx: &mut AsyncFrontendEventReader,
    handle: ClientHandle,
) -> Result<(), CliError> {
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        match rx.next_event(Some(remaining)).await? {
            None => return Ok(()),
            Some(FrontendEvent::Error(message)) => return Err(CliError::ServiceError(message)),
            Some(FrontendEvent::NoSuchClient(id)) if id == handle => {
                return Err(CliError::NoSuchClient(id));
            }
            Some(_) => continue,
        }
    }
}

async fn execute(cmd: CliSubcommand) -> Result<(), CliError> {
    let (mut rx, mut tx) = connect_async(Some(Duration::from_millis(500))).await?;
    tx.request(FrontendRequest::Hello {
        ipc_version: lan_mouse_ipc::IPC_VERSION,
    })
    .await?;
    match cmd {
        CliSubcommand::AddClient(Client {
            hostname,
            port,
            ips,
            position,
            activate,
            enter_hook,
            leave_hook,
            sensitivity,
            fingerprint,
        }) => {
            let port = match port {
                Some(port) => Some(validate_port(port)?),
                None => None,
            };
            let sensitivity = match sensitivity {
                Some(percent) => Some(validate_sensitivity(percent)?),
                None => None,
            };
            let ips = match ips {
                Some(ips) => Some(validate_ips(ips)?),
                None => None,
            };
            // An empty hostname is not a name: treat it as "clear".
            let hostname = hostname.filter(|name| !name.trim().is_empty());
            tx.request(FrontendRequest::Create).await?;
            loop {
                let FrontendEvent::Created(handle, _, _) = next_event(&mut rx).await? else {
                    continue;
                };
                if let Some(hostname) = hostname {
                    tx.request(FrontendRequest::UpdateHostname(handle, Some(hostname)))
                        .await?;
                }
                if let Some(port) = port {
                    tx.request(FrontendRequest::UpdatePort(handle, port))
                        .await?;
                }
                if let Some(ips) = ips {
                    tx.request(FrontendRequest::UpdateFixIps(handle, ips))
                        .await?;
                }
                if let Some(position) = position {
                    tx.request(FrontendRequest::UpdatePosition(handle, position))
                        .await?;
                }
                if let Some(enter_hook) = enter_hook {
                    tx.request(FrontendRequest::UpdateEnterHook(handle, Some(enter_hook)))
                        .await?;
                }
                if let Some(leave_hook) = leave_hook {
                    tx.request(FrontendRequest::UpdateLeaveHook(handle, Some(leave_hook)))
                        .await?;
                }
                if let Some(sensitivity) = sensitivity {
                    tx.request(FrontendRequest::UpdateSensitivity(handle, sensitivity))
                        .await?;
                }
                if let Some(fingerprint) = fingerprint {
                    tx.request(FrontendRequest::UpdateFingerprint(
                        handle,
                        Some(fingerprint),
                    ))
                    .await?;
                }
                // Surface a rejected update (for example an invalid pinned
                // fingerprint) instead of claiming success: the first State
                // event only proves the first update was accepted, so wait out
                // the whole batch window before activating.
                settle_updates(&mut rx, handle).await?;
                if activate {
                    tx.request(FrontendRequest::Activate(handle, true)).await?;
                    settle(&mut rx, Some(handle)).await?;
                    println!(
                        "client {handle} activated; if this device is not authorized on \
                         the peer yet, approve the request on that machine"
                    );
                }
                break;
            }
        }
        CliSubcommand::RemoveClient { id } => {
            ensure_client_exists(&mut rx, &mut tx, id).await?;
            tx.request(FrontendRequest::Delete(id)).await?;
            settle(&mut rx, Some(id)).await?;
        }
        CliSubcommand::Activate { id } => {
            ensure_client_exists(&mut rx, &mut tx, id).await?;
            tx.request(FrontendRequest::Activate(id, true)).await?;
            settle(&mut rx, Some(id)).await?;
        }
        CliSubcommand::Deactivate { id } => {
            ensure_client_exists(&mut rx, &mut tx, id).await?;
            tx.request(FrontendRequest::Activate(id, false)).await?;
            settle(&mut rx, Some(id)).await?;
        }
        CliSubcommand::List => {
            tx.request(FrontendRequest::Enumerate()).await?;
            loop {
                let FrontendEvent::Enumerate(clients) = next_event(&mut rx).await? else {
                    continue;
                };
                if clients.is_empty() {
                    println!("no clients configured");
                }
                for (handle, config, state) in clients {
                    let host = config.hostname.unwrap_or("unknown".to_owned());
                    let port = config.port;
                    let pos = config.pos;
                    let active = state.active;
                    let ips = state.ips;
                    let status = match (
                        state.active,
                        state.active_addr,
                        state.alive,
                        state.incoming_addr,
                    ) {
                        (true, Some(out), true, Some(inc)) => {
                            format!("connected (bidirectional: out={out}, in={inc})")
                        }
                        (true, Some(out), true, None) => format!("connected (to {out})"),
                        (true, _, _, Some(inc)) => format!("connected (from {inc})"),
                        (true, None, _, None) => "waiting-approval".to_string(),
                        (true, Some(out), false, None) => format!("reconnecting ({out})"),
                        (false, _, _, Some(inc)) => format!("receiving-only (from {inc})"),
                        (false, _, _, None) => "disconnected".to_string(),
                    };
                    let rtt = state
                        .rtt_ms
                        .map(|ms| format!(", rtt: {ms}ms"))
                        .unwrap_or_default();
                    println!(
                        "id {handle}: {host}:{port} ({pos}) active: {active}, status: {status}{rtt}, ips: {ips:?}"
                    );
                }
                break;
            }
        }
        CliSubcommand::Scan => {
            tx.request(FrontendRequest::ScanNetwork).await?;
            loop {
                let FrontendEvent::NetworkScan(devices) = next_event(&mut rx).await? else {
                    continue;
                };
                if devices.is_empty() {
                    println!("no devices found on the local network");
                }
                for device in devices {
                    let name = device.hostname.unwrap_or_else(|| "-".to_string());
                    let lm = if device.lan_mouse { "lan-mouse" } else { "-" };
                    let configured = if device.configured { "configured" } else { "" };
                    let version = device.version.unwrap_or_default();
                    println!(
                        "{:<15} {:<20} {:<8} {:<10} {:<8} {}",
                        device.ip,
                        name,
                        device.link.to_string(),
                        lm,
                        version,
                        configured
                    );
                }
                break;
            }
        }
        CliSubcommand::Trust {
            address,
            incoming,
            outgoing,
        } => {
            let incoming = parse_policy(&incoming)?;
            let outgoing = parse_policy(&outgoing)?;
            tx.request(FrontendRequest::AddAddressRule {
                address: address.clone(),
                incoming,
                outgoing,
            })
            .await?;
            settle(&mut rx, None).await?;
            println!(
                "trusted {address}: incoming [{}], outgoing [{}]",
                incoming.capabilities.summary(),
                outgoing.capabilities.summary()
            );
        }
        CliSubcommand::Trusted => {
            tx.request(FrontendRequest::ListAddressRules).await?;
            loop {
                let FrontendEvent::AddressRules(rules) = next_event(&mut rx).await? else {
                    continue;
                };
                if rules.is_empty() {
                    println!("no address rules");
                }
                for rule in rules {
                    println!(
                        "{:<24} in [{}]{} out [{}]{}{}",
                        rule.address,
                        rule.incoming.capabilities.summary(),
                        if rule.incoming.allowed {
                            ""
                        } else {
                            " (disabled)"
                        },
                        rule.outgoing.capabilities.summary(),
                        if rule.outgoing.allowed {
                            ""
                        } else {
                            " (disabled)"
                        },
                        if rule.broad { " (broad!)" } else { "" }
                    );
                }
                break;
            }
        }
        CliSubcommand::Pair {
            seconds,
            incoming,
            outgoing,
        } => {
            let incoming = parse_policy(&incoming)?;
            let outgoing = parse_policy(&outgoing)?;
            tx.request(FrontendRequest::StartPairingScoped {
                seconds,
                incoming,
                outgoing,
            })
            .await?;
            settle(&mut rx, None).await?;
            println!(
                "pairing open for {seconds}s: incoming [{}], outgoing [{}]",
                incoming.capabilities.summary(),
                outgoing.capabilities.summary()
            );
        }
        CliSubcommand::Untrust { address } => {
            tx.request(FrontendRequest::UntrustAddress(address.clone()))
                .await?;
            settle(&mut rx, None).await?;
            println!("removed {address} from the allowlist");
        }
        CliSubcommand::SetHost { id, host } => {
            ensure_client_exists(&mut rx, &mut tx, id).await?;
            // An empty hostname means "clear".
            let host = host.filter(|name| !name.trim().is_empty());
            tx.request(FrontendRequest::UpdateHostname(id, host))
                .await?;
            settle(&mut rx, Some(id)).await?;
        }
        CliSubcommand::SetPort { id, port } => {
            ensure_client_exists(&mut rx, &mut tx, id).await?;
            let port = validate_port(port)?;
            tx.request(FrontendRequest::UpdatePort(id, port)).await?;
            settle(&mut rx, Some(id)).await?;
        }
        CliSubcommand::SetPosition { id, pos } => {
            ensure_client_exists(&mut rx, &mut tx, id).await?;
            tx.request(FrontendRequest::UpdatePosition(id, pos)).await?;
            settle(&mut rx, Some(id)).await?;
        }
        CliSubcommand::SetSensitivity { id, percent } => {
            ensure_client_exists(&mut rx, &mut tx, id).await?;
            let percent = validate_sensitivity(percent)?;
            tx.request(FrontendRequest::UpdateSensitivity(id, percent))
                .await?;
            settle(&mut rx, Some(id)).await?;
        }
        CliSubcommand::SetAbsoluteSensitivity { percent } => {
            let percent = validate_sensitivity(percent)?;
            tx.request(FrontendRequest::SetAbsoluteSensitivity(percent))
                .await?;
            settle(&mut rx, None).await?;
        }
        CliSubcommand::Devices => {
            tx.request(FrontendRequest::ListDevices).await?;
            loop {
                let FrontendEvent::Devices(devices) = next_event(&mut rx).await? else {
                    continue;
                };
                if devices.is_empty() {
                    println!("no devices known yet");
                }
                for device in devices {
                    let short = device.fingerprint.chars().take(23).collect::<String>();
                    println!(
                        "{:<20} {:<26} {:<7} in:[{}]{} out:[{}]{} {}{}",
                        device.name,
                        short,
                        device.position,
                        device.incoming.capabilities.summary(),
                        if device.incoming.allowed {
                            ""
                        } else {
                            " (disabled)"
                        },
                        device.outgoing.capabilities.summary(),
                        if device.outgoing.allowed {
                            ""
                        } else {
                            " (disabled)"
                        },
                        device.source,
                        if device.connected {
                            " [connected]"
                        } else if device.active {
                            " [active]"
                        } else {
                            ""
                        }
                    );
                }
                break;
            }
        }
        CliSubcommand::Pending => {
            tx.request(FrontendRequest::ListPending).await?;
            loop {
                let FrontendEvent::PendingRequests(pending) = next_event(&mut rx).await? else {
                    continue;
                };
                if pending.is_empty() {
                    println!("no pending connection requests");
                }
                for request in pending {
                    println!(
                        "{:<26} from {}{} ({} attempt(s)) code={}{}",
                        request.fingerprint,
                        request.addr,
                        request
                            .name
                            .as_deref()
                            .map(|name| format!(" as {name}"))
                            .unwrap_or_default(),
                        request.attempts,
                        request.sas,
                        request
                            .previous_fingerprint
                            .as_deref()
                            .map(|old| format!(
                                " WARNING: certificate changed (was {})",
                                old.chars().take(23).collect::<String>()
                            ))
                            .unwrap_or_default()
                    );
                }
                break;
            }
        }
        CliSubcommand::Allow {
            fingerprint,
            name,
            position,
            incoming,
            outgoing,
            once,
        } => {
            let incoming = parse_policy(&incoming)?;
            if once {
                tx.request(FrontendRequest::GrantSession {
                    fingerprint: fingerprint.clone(),
                    name,
                    position: Some(position),
                    incoming,
                })
                .await?;
                settle(&mut rx, None).await?;
                println!(
                    "granted session access to {fingerprint}: incoming [{}] (dropped on disconnect)",
                    incoming.capabilities.summary()
                );
            } else {
                let outgoing = parse_policy(&outgoing)?;
                tx.request(FrontendRequest::RespondPending {
                    fingerprint: fingerprint.clone(),
                    accept: true,
                    name,
                    position: Some(position),
                    incoming: Some(incoming),
                    outgoing: Some(outgoing),
                })
                .await?;
                settle(&mut rx, None).await?;
                println!(
                    "allowed {fingerprint}: incoming [{}], outgoing [{}]",
                    incoming.capabilities.summary(),
                    outgoing.capabilities.summary()
                );
            }
        }
        CliSubcommand::Deny { fingerprint, r#for } => {
            let expires_in = parse_duration(&r#for)?;
            tx.request(FrontendRequest::DenyDevice {
                fingerprint: fingerprint.clone(),
                expires_in,
            })
            .await?;
            settle(&mut rx, None).await?;
            match expires_in {
                Some(seconds) => println!("denied {fingerprint} for {seconds}s"),
                None => println!("denied {fingerprint} permanently"),
            }
        }
        CliSubcommand::Revoke { fingerprint } => {
            tx.request(FrontendRequest::RevokeDevice {
                fingerprint,
                disconnect: true,
            })
            .await?;
            settle(&mut rx, None).await?;
        }
        CliSubcommand::Rename { fingerprint, name } => {
            tx.request(FrontendRequest::RenameDevice { fingerprint, name })
                .await?;
            settle(&mut rx, None).await?;
        }
        CliSubcommand::History { limit } => {
            // The service clamps to 1..=500; mirror it so the output matches
            // what was requested and a huge/zero value is not silently
            // changed.
            let effective = limit.clamp(1, 500);
            if effective != limit {
                eprintln!(
                    "warning: history limit {limit} is out of range, using {effective} (1..=500)"
                );
            }
            tx.request(FrontendRequest::ListHistory { limit: effective })
                .await?;
            loop {
                let FrontendEvent::ConnectionHistory(history) = next_event(&mut rx).await? else {
                    continue;
                };
                if history.is_empty() {
                    println!("no audit events recorded yet");
                }
                for entry in history {
                    println!(
                        "{} {}{}{}{}",
                        entry.ts,
                        entry.event,
                        entry
                            .name
                            .as_deref()
                            .map(|name| format!(" name={name}"))
                            .unwrap_or_default(),
                        entry
                            .fingerprint
                            .as_deref()
                            .map(|fp| format!(" fp={fp}"))
                            .unwrap_or_default(),
                        entry
                            .detail
                            .as_deref()
                            .map(|detail| format!(" {detail}"))
                            .unwrap_or_default()
                    );
                }
                break;
            }
        }
        CliSubcommand::ExportPolicy { file } => {
            tx.request(FrontendRequest::ExportPolicy).await?;
            loop {
                let FrontendEvent::PolicyExport { json } = next_event(&mut rx).await? else {
                    continue;
                };
                std::fs::write(&file, json)?;
                println!("policy exported to {}", file.display());
                break;
            }
        }
        CliSubcommand::ImportPolicy { file } => {
            let json = std::fs::read_to_string(&file)?;
            tx.request(FrontendRequest::ImportPolicy { json }).await?;
            settle(&mut rx, None).await?;
            println!("policy imported from {}", file.display());
        }
        CliSubcommand::SetFingerprint { id, fingerprint } => {
            ensure_client_exists(&mut rx, &mut tx, id).await?;
            tx.request(FrontendRequest::UpdateFingerprint(id, fingerprint.clone()))
                .await?;
            settle(&mut rx, Some(id)).await?;
            match fingerprint {
                Some(fp) => println!("client {id}: pinned fingerprint {fp}"),
                None => println!("client {id}: fingerprint cleared (trust on first use)"),
            }
        }
        CliSubcommand::SetIps { id, ips } => {
            ensure_client_exists(&mut rx, &mut tx, id).await?;
            let ips = validate_ips(ips)?;
            tx.request(FrontendRequest::UpdateFixIps(id, ips)).await?;
            settle(&mut rx, Some(id)).await?;
        }
        CliSubcommand::EnableCapture => {
            tx.request(FrontendRequest::EnableCapture).await?;
            settle(&mut rx, None).await?;
        }
        CliSubcommand::EnableEmulation => {
            tx.request(FrontendRequest::EnableEmulation).await?;
            settle(&mut rx, None).await?;
        }
        CliSubcommand::AuthorizeKey {
            description,
            sha256_fingerprint,
        } => {
            tx.request(FrontendRequest::AuthorizeKey(
                description,
                sha256_fingerprint,
            ))
            .await?;
            settle(&mut rx, None).await?;
        }
        CliSubcommand::AuthorizeDevice {
            sha256_fingerprint,
            name,
            position,
        } => {
            let name = name.unwrap_or_default();
            tx.request(FrontendRequest::AuthorizeDevice {
                fingerprint: sha256_fingerprint,
                name,
                position,
            })
            .await?;
            settle(&mut rx, None).await?;
            println!(
                "device authorized; a client at position {position} is created and \
                 activated as soon as it connects"
            );
        }
        CliSubcommand::RemoveAuthorizedKey { sha256_fingerprint } => {
            tx.request(FrontendRequest::RemoveAuthorizedKey(sha256_fingerprint))
                .await?;
            settle(&mut rx, None).await?;
        }
        CliSubcommand::SaveConfig => {
            tx.request(FrontendRequest::SaveConfiguration).await?;
            settle(&mut rx, None).await?;
        }
        CliSubcommand::Clipboard { action } => match action {
            ClipboardAction::Status => {
                // `Sync` makes the service broadcast the current status.
                tx.request(FrontendRequest::Sync).await?;
                loop {
                    let FrontendEvent::ClipboardStatus(status) = next_event(&mut rx).await? else {
                        continue;
                    };
                    println!("enabled:   {}", status.enabled);
                    println!(
                        "backend:   {}",
                        status.backend.unwrap_or_else(|| "-".into())
                    );
                    println!("available: {}", status.available);
                    println!("received:  {} snapshot(s) from peers", status.applied);
                    println!("detected:  {} local change(s)", status.detected);
                    match status.last_error {
                        Some(error) => println!("last error: {error}"),
                        None => println!("last error: -"),
                    }
                    return Ok(());
                }
            }
            action => {
                tx.request(FrontendRequest::SetClipboardEnabled(matches!(
                    action,
                    ClipboardAction::Enable
                )))
                .await?;
                settle(&mut rx, None).await?;
            }
        },
        CliSubcommand::Shutdown => {
            tx.request(FrontendRequest::Shutdown).await?;
            // The service closes the IPC connection during its graceful
            // teardown; waiting for that close lets scripts (and the Windows
            // uninstaller) rely on the service being gone when this returns.
            let deadline = Instant::now() + SHUTDOWN_WAIT;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(CliError::ResponseTimeout(SHUTDOWN_WAIT));
                }
                match rx.next_event(Some(remaining)).await {
                    Ok(None) => return Err(CliError::ResponseTimeout(SHUTDOWN_WAIT)),
                    Ok(Some(FrontendEvent::Error(message))) => {
                        return Err(CliError::ServiceError(message));
                    }
                    // The connection closing is the success signal.
                    Err(IpcError::Io(_)) => break,
                    Err(e) => return Err(CliError::Ipc(e)),
                    Ok(Some(_)) => continue,
                }
            }
            println!("lan-mouse service is shutting down");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> CliArgs {
        CliArgs::try_parse_from(args).expect("parse arguments")
    }

    #[test]
    fn add_client_defaults_are_conservative() {
        let args = parse(&["lan-mouse-cli", "add-client"]);
        match args.command {
            CliSubcommand::AddClient(client) => {
                assert_eq!(client.hostname, None);
                assert_eq!(client.port, None);
                assert_eq!(client.ips, None);
                assert_eq!(client.position, None);
                assert!(!client.activate);
                assert_eq!(client.enter_hook, None);
                assert_eq!(client.leave_hook, None);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn add_client_accepts_position_and_activate() {
        let args = parse(&[
            "lan-mouse-cli",
            "add-client",
            "--hostname",
            "desktop",
            "--ips",
            "192.168.1.10",
            "--position",
            "left",
            "--activate",
        ]);
        match args.command {
            CliSubcommand::AddClient(client) => {
                assert_eq!(client.hostname.as_deref(), Some("desktop"));
                assert_eq!(
                    client.ips,
                    Some(vec![IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 10))])
                );
                assert_eq!(client.position, Some(Position::Left));
                assert!(client.activate);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn every_position_value_is_accepted() {
        for (text, expected) in [
            ("left", Position::Left),
            ("right", Position::Right),
            ("top", Position::Top),
            ("bottom", Position::Bottom),
        ] {
            let args = parse(&["lan-mouse-cli", "add-client", "--position", text]);
            match args.command {
                CliSubcommand::AddClient(client) => assert_eq!(client.position, Some(expected)),
                other => panic!("unexpected command: {other:?}"),
            }
        }
    }

    #[test]
    fn invalid_position_is_rejected() {
        assert!(
            CliArgs::try_parse_from(["lan-mouse-cli", "add-client", "--position", "diagonal"])
                .is_err()
        );
    }

    #[test]
    fn activate_and_deactivate_parse_handles() {
        let args = parse(&["lan-mouse-cli", "activate", "7"]);
        assert!(matches!(args.command, CliSubcommand::Activate { id: 7 }));
        let args = parse(&["lan-mouse-cli", "deactivate", "9"]);
        assert!(matches!(args.command, CliSubcommand::Deactivate { id: 9 }));
    }

    #[test]
    fn clipboard_actions_parse() {
        let args = parse(&["lan-mouse-cli", "clipboard", "enable"]);
        assert!(matches!(
            args.command,
            CliSubcommand::Clipboard {
                action: ClipboardAction::Enable
            }
        ));
        let args = parse(&["lan-mouse-cli", "clipboard", "disable"]);
        assert!(matches!(
            args.command,
            CliSubcommand::Clipboard {
                action: ClipboardAction::Disable
            }
        ));
        let args = parse(&["lan-mouse-cli", "clipboard", "status"]);
        assert!(matches!(
            args.command,
            CliSubcommand::Clipboard {
                action: ClipboardAction::Status
            }
        ));
        assert!(CliArgs::try_parse_from(["lan-mouse-cli", "clipboard", "maybe"]).is_err());
    }

    #[test]
    fn shutdown_subcommand_parses() {
        let args = parse(&["lan-mouse-cli", "shutdown"]);
        assert!(matches!(args.command, CliSubcommand::Shutdown));
    }

    #[test]
    fn invalid_ports_and_sensitivities_are_rejected() {
        assert!(validate_port(0).is_err());
        assert_eq!(validate_port(4242).expect("valid port"), 4242);
        assert!(validate_sensitivity(9).is_err());
        assert!(validate_sensitivity(501).is_err());
        assert_eq!(validate_sensitivity(100).expect("valid"), 100);
        assert_eq!(validate_sensitivity(500).expect("valid"), 500);
    }

    #[test]
    fn deny_durations_do_not_overflow() {
        assert_eq!(parse_duration("forever").expect("forever"), None);
        assert_eq!(parse_duration("30m").expect("30m"), Some(1800));
        assert_eq!(parse_duration("2h").expect("2h"), Some(7200));
        assert_eq!(parse_duration("1d").expect("1d"), Some(86400));
        // Would overflow u64 when converted to seconds: must be rejected, not
        // wrap or panic.
        assert!(parse_duration("18446744073709551615d").is_err());
        assert!(parse_duration("999999999999999999999999s").is_err());
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        assert!(CliArgs::try_parse_from(["lan-mouse-cli", "teleport"]).is_err());
    }
}
