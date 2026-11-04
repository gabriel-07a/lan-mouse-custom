use clap::{Args, Parser, Subcommand, ValueEnum};
use futures::StreamExt;

use std::{net::IpAddr, time::Duration};
use thiserror::Error;

use lan_mouse_ipc::{
    ClientHandle, ConnectionError, FrontendEvent, FrontendRequest, IpcError, Position,
    connect_async,
};

#[derive(Debug, Error)]
pub enum CliError {
    /// is the service running?
    #[error("could not connect: `{0}` - is the service running?")]
    ServiceNotRunning(#[from] ConnectionError),
    #[error("error communicating with service: {0}")]
    Ipc(#[from] IpcError),
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
    /// change hostname
    SetHost {
        id: ClientHandle,
        host: Option<String>,
    },
    /// change port
    SetPort { id: ClientHandle, port: u16 },
    /// set position
    SetPosition { id: ClientHandle, pos: Position },
    /// set ips
    SetIps { id: ClientHandle, ips: Vec<IpAddr> },
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
}

pub async fn run(args: CliArgs) -> Result<(), CliError> {
    execute(args.command).await?;
    Ok(())
}

async fn execute(cmd: CliSubcommand) -> Result<(), CliError> {
    let (mut rx, mut tx) = connect_async(Some(Duration::from_millis(500))).await?;
    match cmd {
        CliSubcommand::AddClient(Client {
            hostname,
            port,
            ips,
            position,
            activate,
            enter_hook,
            leave_hook,
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
                        println!(
                            "id {handle}: {host}:{port} ({pos}) active: {active}, ips: {ips:?}"
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
        CliSubcommand::Clipboard { action } => {
            tx.request(FrontendRequest::SetClipboardEnabled(matches!(
                action,
                ClipboardAction::Enable
            )))
            .await?
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
        assert!(CliArgs::try_parse_from(["lan-mouse-cli", "clipboard", "maybe"]).is_err());
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        assert!(CliArgs::try_parse_from(["lan-mouse-cli", "teleport"]).is_err());
    }
}
