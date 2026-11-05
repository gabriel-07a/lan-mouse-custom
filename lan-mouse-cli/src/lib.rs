use clap::{Args, Parser, Subcommand, ValueEnum};
use futures::StreamExt;

use std::{net::IpAddr, time::Duration};
use thiserror::Error;

use lan_mouse_ipc::{
    Capabilities, ClientHandle, ConnectionError, DirectionPolicy, FrontendEvent, FrontendRequest,
    IpcError, Position, connect_async,
};

#[derive(Debug, Error)]
pub enum CliError {
    /// is the service running?
    #[error("could not connect: `{0}` - is the service running?")]
    ServiceNotRunning(#[from] ConnectionError),
    #[error("error communicating with service: {0}")]
    Ipc(#[from] IpcError),
    #[error("{0}")]
    InvalidArgument(String),
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
    /// remove an address from the allowlist
    Untrust { address: String },
    /// accept any device that connects in the next seconds (default 60)
    Pair {
        #[arg(long, default_value = "60")]
        seconds: u64,
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
    },
    /// deny a pending device permanently (revoked devices are never asked again)
    Deny {
        /// SHA-256 fingerprint of the device
        fingerprint: String,
    },
    /// revoke a device: closes live sessions and denies the fingerprint
    Revoke {
        /// SHA-256 fingerprint of the device
        fingerprint: String,
    },
    /// rename a known device
    Rename { fingerprint: String, name: String },
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
            tx.request(FrontendRequest::Create).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Created(handle, _, _) = e? {
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
                    if activate {
                        tx.request(FrontendRequest::Activate(handle, true)).await?;
                        println!(
                            "client {handle} activated; if this device is not authorized on \
                             the peer yet, approve the request on that machine"
                        );
                    }
                    break;
                }
            }
        }
        CliSubcommand::RemoveClient { id } => tx.request(FrontendRequest::Delete(id)).await?,
        CliSubcommand::Activate { id } => tx.request(FrontendRequest::Activate(id, true)).await?,
        CliSubcommand::Deactivate { id } => {
            tx.request(FrontendRequest::Activate(id, false)).await?
        }
        CliSubcommand::List => {
            tx.request(FrontendRequest::Enumerate()).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Enumerate(clients) = e? {
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
        }
        CliSubcommand::Scan => {
            tx.request(FrontendRequest::ScanNetwork).await?;
            while let Some(event) = rx.next().await {
                if let FrontendEvent::NetworkScan(devices) = event? {
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
            println!(
                "trusted {address}: incoming [{}], outgoing [{}]",
                incoming.capabilities.summary(),
                outgoing.capabilities.summary()
            );
        }
        CliSubcommand::Trusted => {
            tx.request(FrontendRequest::ListAddressRules).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::AddressRules(rules) = e? {
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
        }
        CliSubcommand::Pair { seconds } => {
            tx.request(FrontendRequest::StartPairing(seconds)).await?;
            println!(
                "pairing mode open for {seconds}s: any device that connects is authorized \
                 and gets a client entry for the reverse direction"
            );
        }
        CliSubcommand::Untrust { address } => {
            tx.request(FrontendRequest::UntrustAddress(address.clone()))
                .await?;
            println!("removed {address} from the allowlist");
        }
        CliSubcommand::SetHost { id, host } => {
            tx.request(FrontendRequest::UpdateHostname(id, host))
                .await?
        }
        CliSubcommand::SetPort { id, port } => {
            tx.request(FrontendRequest::UpdatePort(id, port)).await?
        }
        CliSubcommand::SetPosition { id, pos } => {
            tx.request(FrontendRequest::UpdatePosition(id, pos)).await?
        }
        CliSubcommand::SetSensitivity { id, percent } => {
            tx.request(FrontendRequest::UpdateSensitivity(id, percent))
                .await?
        }
        CliSubcommand::Devices => {
            tx.request(FrontendRequest::ListDevices).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Devices(devices) = e? {
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
        }
        CliSubcommand::Pending => {
            tx.request(FrontendRequest::ListPending).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::PendingRequests(pending) = e? {
                    if pending.is_empty() {
                        println!("no pending connection requests");
                    }
                    for request in pending {
                        println!(
                            "{:<26} from {}{} ({} attempt(s))",
                            request.fingerprint,
                            request.addr,
                            request
                                .name
                                .as_deref()
                                .map(|name| format!(" as {name}"))
                                .unwrap_or_default(),
                            request.attempts
                        );
                    }
                    break;
                }
            }
        }
        CliSubcommand::Allow {
            fingerprint,
            name,
            position,
            incoming,
            outgoing,
        } => {
            let incoming = parse_policy(&incoming)?;
            let outgoing = parse_policy(&outgoing)?;
            tx.request(FrontendRequest::RespondPending {
                fingerprint,
                accept: true,
                name,
                position: Some(position),
                incoming: Some(incoming),
                outgoing: Some(outgoing),
            })
            .await?;
        }
        CliSubcommand::Deny { fingerprint } => {
            tx.request(FrontendRequest::RespondPending {
                fingerprint,
                accept: false,
                name: None,
                position: None,
                incoming: None,
                outgoing: None,
            })
            .await?;
        }
        CliSubcommand::Revoke { fingerprint } => {
            tx.request(FrontendRequest::RevokeDevice {
                fingerprint,
                disconnect: true,
            })
            .await?;
        }
        CliSubcommand::Rename { fingerprint, name } => {
            tx.request(FrontendRequest::RenameDevice { fingerprint, name })
                .await?;
        }
        CliSubcommand::SetFingerprint { id, fingerprint } => {
            tx.request(FrontendRequest::UpdateFingerprint(id, fingerprint.clone()))
                .await?;
            match fingerprint {
                Some(fp) => println!("client {id}: pinned fingerprint {fp}"),
                None => println!("client {id}: fingerprint cleared (trust on first use)"),
            }
        }
        CliSubcommand::SetIps { id, ips } => {
            tx.request(FrontendRequest::UpdateFixIps(id, ips)).await?
        }
        CliSubcommand::EnableCapture => tx.request(FrontendRequest::EnableCapture).await?,
        CliSubcommand::EnableEmulation => tx.request(FrontendRequest::EnableEmulation).await?,
        CliSubcommand::AuthorizeKey {
            description,
            sha256_fingerprint,
        } => {
            tx.request(FrontendRequest::AuthorizeKey(
                description,
                sha256_fingerprint,
            ))
            .await?
        }
        CliSubcommand::AuthorizeDevice {
            sha256_fingerprint,
            name,
            position,
        } => {
            tx.request(FrontendRequest::AuthorizeDevice {
                fingerprint: sha256_fingerprint,
                name: name.unwrap_or_default(),
                position,
            })
            .await?;
            println!(
                "device authorized; a client at position {position} is created and \
                 activated as soon as it connects"
            );
        }
        CliSubcommand::RemoveAuthorizedKey { sha256_fingerprint } => {
            tx.request(FrontendRequest::RemoveAuthorizedKey(sha256_fingerprint))
                .await?
        }
        CliSubcommand::SaveConfig => tx.request(FrontendRequest::SaveConfiguration).await?,
        CliSubcommand::Clipboard { action } => match action {
            ClipboardAction::Status => {
                // `Sync` makes the service broadcast the current status.
                tx.request(FrontendRequest::Sync).await?;
                while let Some(event) = rx.next().await {
                    if let FrontendEvent::ClipboardStatus(status) = event? {
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
            }
            action => {
                tx.request(FrontendRequest::SetClipboardEnabled(matches!(
                    action,
                    ClipboardAction::Enable
                )))
                .await?
            }
        },
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
    fn unknown_subcommand_is_rejected() {
        assert!(CliArgs::try_parse_from(["lan-mouse-cli", "teleport"]).is_err());
    }
}
