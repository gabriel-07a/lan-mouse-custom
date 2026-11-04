use std::{
    collections::{HashMap, HashSet},
    env::VarError,
    fmt::Display,
    io,
    net::{IpAddr, SocketAddr},
    str::FromStr,
};
use thiserror::Error;

#[cfg(unix)]
use std::{
    env,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

mod connect;
mod connect_async;
mod listen;

pub use connect::{FrontendEventReader, FrontendRequestWriter, connect, try_connect};
#[cfg(unix)]
pub use connect_async::connect_async_at;
pub use connect_async::{AsyncFrontendEventReader, AsyncFrontendRequestWriter, connect_async};
pub use listen::AsyncFrontendListener;

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error(transparent)]
    SocketPath(#[from] SocketPathError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("connection timed out")]
    Timeout,
}

#[derive(Debug, Error)]
pub enum IpcListenerCreationError {
    #[error("could not determine socket-path: `{0}`")]
    SocketPath(#[from] SocketPathError),
    #[error("service already running!")]
    AlreadyRunning,
    #[error("failed to bind lan-mouse socket: `{0}`")]
    Bind(io::Error),
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("io error occured: `{0}`")]
    Io(#[from] io::Error),
    #[error("invalid json: `{0}`")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Listen(#[from] IpcListenerCreationError),
}

pub const DEFAULT_PORT: u16 = 4242;

#[derive(Debug, Default, Eq, Hash, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Position::Right,
            Position::Right => Position::Left,
            Position::Top => Position::Bottom,
            Position::Bottom => Position::Top,
        }
    }
}

#[derive(Debug, Error)]
#[error("not a valid position: {pos}")]
pub struct PositionParseError {
    pos: String,
}

impl FromStr for Position {
    type Err = PositionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "top" => Ok(Self::Top),
            "bottom" => Ok(Self::Bottom),
            _ => Err(PositionParseError { pos: s.into() }),
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Position::Left => "left",
                Position::Right => "right",
                Position::Top => "top",
                Position::Bottom => "bottom",
            }
        )
    }
}

impl TryFrom<&str> for Position {
    type Error = ();

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "left" => Ok(Position::Left),
            "right" => Ok(Position::Right),
            "top" => Ok(Position::Top),
            "bottom" => Ok(Position::Bottom),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// hostname of this client
    pub hostname: Option<String>,
    /// fix ips, determined by the user
    pub fix_ips: Vec<IpAddr>,
    /// both active_addr and addrs can be None / empty so port needs to be stored seperately
    pub port: u16,
    /// position of a client on screen
    pub pos: Position,
    /// enter hook
    pub cmd: Option<String>,
    /// leave hook
    pub leave_cmd: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            hostname: Default::default(),
            fix_ips: Default::default(),
            pos: Default::default(),
            cmd: None,
            leave_cmd: None,
        }
    }
}

pub type ClientHandle = u64;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ClientState {
    /// events should be sent to and received from the client
    pub active: bool,
    /// `active` address of the client, used to send data to.
    /// This should generally be the socket address where data
    /// was last received from.
    pub active_addr: Option<SocketAddr>,
    /// tracks whether or not the client is available for emulation
    pub alive: bool,
    /// smoothed round-trip time to the peer in milliseconds, if known
    #[serde(default)]
    pub rtt_ms: Option<u32>,
    /// ips from dns
    pub dns_ips: Vec<IpAddr>,
    /// all ip addresses associated with a particular client
    /// e.g. Laptops usually have at least an ethernet and a wifi port
    /// which have different ip addresses
    pub ips: HashSet<IpAddr>,
    /// client has pressed keys
    pub has_pressed_keys: bool,
    /// dns resolving in progress
    pub resolving: bool,
    /// Peer's build short commit hash from the [`Hello`] proto
    /// event. `None` means we haven't received a Hello yet — either
    /// the connection is fresh, or the peer is on an older build
    /// that predates the Hello event. The frontend uses this to
    /// soft-warn on version mismatch.
    pub peer_commit: Option<[u8; 8]>,
    /// wire protocol version advertised by the peer (0 for legacy peers)
    #[serde(default)]
    pub peer_proto_version: u8,
    /// optional protocol features advertised by the peer
    #[serde(default)]
    pub peer_capabilities: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FrontendEvent {
    /// a client was created
    Created(ClientHandle, ClientConfig, ClientState),
    /// no such client
    NoSuchClient(ClientHandle),
    /// state changed
    State(ClientHandle, ClientConfig, ClientState),
    /// the client was deleted
    Deleted(ClientHandle),
    /// new port, reason of failure (if failed)
    PortChanged(u16, Option<String>),
    /// list of all clients, used for initial state synchronization
    Enumerate(Vec<(ClientHandle, ClientConfig, ClientState)>),
    /// an error occured
    Error(String),
    /// capture status
    CaptureStatus(Status),
    /// emulation status
    EmulationStatus(Status),
    /// authorized public key fingerprints have been updated
    AuthorizedUpdated(HashMap<String, String>),
    /// public key fingerprint of this device
    PublicKeyFingerprint(String),
    /// new device connected
    DeviceConnected {
        addr: SocketAddr,
        fingerprint: String,
    },
    /// incoming device entered the screen
    DeviceEntered {
        fingerprint: String,
        addr: SocketAddr,
        pos: Position,
    },
    /// incoming disconnected
    IncomingDisconnected(SocketAddr),
    /// failed connection attempt (approval for fingerprint required)
    ConnectionAttempt { fingerprint: String },
    /// runtime diagnostics (backends, protocol version, …)
    Diagnostics(Diagnostics),
    /// clipboard synchronization status
    ClipboardStatus(ClipboardStatus),
}

/// Runtime state of the clipboard integration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipboardStatus {
    /// user-controlled toggle
    pub enabled: bool,
    /// selected backend (`portal`, `windows`, `macos`, …)
    pub backend: Option<String>,
    /// whether a backend could be initialized
    pub available: bool,
    /// number of snapshots applied from a peer since startup
    pub applied: u64,
    /// last error, if any
    pub last_error: Option<String>,
}

impl ClipboardStatus {
    /// One-line summary for the UI and logs.
    pub fn summary(&self) -> String {
        if !self.enabled {
            return "disabled".to_string();
        }
        if !self.available {
            return "backend unavailable".to_string();
        }
        let backend = self.backend.as_deref().unwrap_or("unknown backend");
        let mut summary = format!("{backend} · {} synced", self.applied);
        if let Some(error) = &self.last_error {
            summary.push_str(&format!(" · {error}"));
        }
        summary
    }
}

/// Runtime information about the local instance, used to diagnose why a
/// device can (or cannot) send or receive input.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Diagnostics {
    /// input-capture backend actually in use, if any
    pub capture_backend: Option<String>,
    /// input-emulation backend actually in use, if any
    pub emulation_backend: Option<String>,
    /// desktop session type (e.g. `wayland` / `x11`), Unix only
    pub session_type: Option<String>,
    /// desktop environment (e.g. `GNOME`), Unix only
    pub desktop: Option<String>,
    /// listen port
    pub port: u16,
    /// wire protocol version of this build
    pub proto_version: u8,
    /// protocol features implemented by this build
    pub capabilities: u32,
    /// clipboard backend in use, if any
    #[serde(default)]
    pub clipboard_backend: Option<String>,
    /// whether clipboard synchronization is enabled
    #[serde(default)]
    pub clipboard_enabled: bool,
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub enum FrontendRequest {
    /// activate/deactivate client
    Activate(ClientHandle, bool),
    /// add a new client
    Create,
    /// change the listen port (recreate udp listener)
    ChangePort(u16),
    /// remove a client
    Delete(ClientHandle),
    /// request an enumeration of all clients
    Enumerate(),
    /// resolve dns
    ResolveDns(ClientHandle),
    /// update hostname
    UpdateHostname(ClientHandle, Option<String>),
    /// update port
    UpdatePort(ClientHandle, u16),
    /// update position
    UpdatePosition(ClientHandle, Position),
    /// update fix-ips
    UpdateFixIps(ClientHandle, Vec<IpAddr>),
    /// request reenabling input capture
    EnableCapture,
    /// request reenabling input emulation
    EnableEmulation,
    /// synchronize all state
    Sync,
    /// authorize fingerprint (description, fingerprint)
    AuthorizeKey(String, String),
    /// remove fingerprint (fingerprint)
    RemoveAuthorizedKey(String),
    /// change the hook command
    UpdateEnterHook(u64, Option<String>),
    /// change the leave hook command
    UpdateLeaveHook(u64, Option<String>),
    /// save config file
    SaveConfiguration,
    /// enable or disable clipboard synchronization
    SetClipboardEnabled(bool),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Status {
    #[default]
    Disabled,
    Enabled,
}

impl From<Status> for bool {
    fn from(status: Status) -> Self {
        match status {
            Status::Enabled => true,
            Status::Disabled => false,
        }
    }
}

#[cfg(unix)]
const LAN_MOUSE_SOCKET_NAME: &str = "lan-mouse-socket.sock";

#[derive(Debug, Error)]
pub enum SocketPathError {
    #[error("could not determine $XDG_RUNTIME_DIR: `{0}`")]
    XdgRuntimeDirNotFound(VarError),
    #[error("could not determine $HOME: `{0}`")]
    HomeDirNotFound(VarError),
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let xdg_runtime_dir =
        env::var("XDG_RUNTIME_DIR").map_err(SocketPathError::XdgRuntimeDirNotFound)?;
    Ok(Path::new(xdg_runtime_dir.as_str()).join(LAN_MOUSE_SOCKET_NAME))
}

#[cfg(all(unix, target_os = "macos"))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let home = env::var("HOME").map_err(SocketPathError::HomeDirNotFound)?;
    Ok(Path::new(home.as_str())
        .join("Library")
        .join("Caches")
        .join(LAN_MOUSE_SOCKET_NAME))
}

/// Check if a lan-mouse service is already running by probing the IPC socket.
#[cfg(unix)]
pub fn is_service_running() -> bool {
    let Ok(socket_path) = default_socket_path() else {
        return false;
    };
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

/// Check if a lan-mouse service is already running by probing the IPC socket.
#[cfg(windows)]
pub fn is_service_running() -> bool {
    std::net::TcpStream::connect("127.0.0.1:5252").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn json_roundtrip<T>(value: T) -> T
    where
        T: Serialize + DeserializeOwned,
    {
        let encoded = serde_json::to_value(&value).expect("serialize");
        let decoded = serde_json::from_value(encoded.clone()).expect("deserialize");
        let re_encoded = serde_json::to_value(&decoded).expect("re-serialize");
        assert_eq!(encoded, re_encoded);
        decoded
    }

    #[test]
    fn position_opposite_is_involutive() {
        for pos in [
            Position::Left,
            Position::Right,
            Position::Top,
            Position::Bottom,
        ] {
            assert_eq!(pos.opposite().opposite(), pos);
            assert_ne!(pos.opposite(), pos);
        }
    }

    #[test]
    fn position_display_and_parse_roundtrip() {
        for (pos, text) in [
            (Position::Left, "left"),
            (Position::Right, "right"),
            (Position::Top, "top"),
            (Position::Bottom, "bottom"),
        ] {
            assert_eq!(pos.to_string(), text);
            assert_eq!(text.parse::<Position>().expect("parse"), pos);
        }
        assert!("diagonal".parse::<Position>().is_err());
    }

    #[test]
    fn position_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Position::Top).expect("ser"),
            "\"top\""
        );
        let decoded: Position = serde_json::from_str("\"right\"").expect("de");
        assert_eq!(decoded, Position::Right);
    }

    #[test]
    fn client_state_roundtrips() {
        let state = ClientState {
            active: true,
            active_addr: Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
                4242,
            )),
            alive: true,
            rtt_ms: Some(12),
            dns_ips: vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3))],
            ips: HashSet::from([IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))]),
            has_pressed_keys: true,
            resolving: false,
            peer_commit: Some(*b"abcd1234"),
            peer_proto_version: 1,
            peer_capabilities: 0b101,
        };
        let decoded = json_roundtrip(state);
        assert_eq!(decoded.rtt_ms, Some(12));
        assert_eq!(decoded.peer_proto_version, 1);
        assert_eq!(decoded.peer_capabilities, 0b101);
    }

    #[test]
    fn client_state_from_an_older_peer_uses_defaults() {
        // Shape emitted by a build predating RTT/Hello extensions.
        let legacy = serde_json::json!({
            "active": true,
            "active_addr": null,
            "alive": false,
            "dns_ips": [],
            "ips": ["192.168.1.10"],
            "has_pressed_keys": false,
            "resolving": false,
            "peer_commit": null
        });
        let state: ClientState = serde_json::from_value(legacy).expect("legacy state");
        assert_eq!(state.rtt_ms, None);
        assert_eq!(state.peer_proto_version, 0);
        assert_eq!(state.peer_capabilities, 0);
    }

    #[test]
    fn frontend_events_roundtrip() {
        json_roundtrip(FrontendEvent::NoSuchClient(7));
        json_roundtrip(FrontendEvent::Deleted(3));
        json_roundtrip(FrontendEvent::Error("boom".into()));
        json_roundtrip(FrontendEvent::PortChanged(4242, None));
        json_roundtrip(FrontendEvent::PortChanged(4243, Some("in use".into())));
        json_roundtrip(FrontendEvent::CaptureStatus(Status::Disabled));
        json_roundtrip(FrontendEvent::EmulationStatus(Status::Enabled));
        json_roundtrip(FrontendEvent::ConnectionAttempt {
            fingerprint: "aa:bb".into(),
        });
        json_roundtrip(FrontendEvent::Diagnostics(Diagnostics {
            capture_backend: Some("libei".into()),
            emulation_backend: Some("wlroots".into()),
            session_type: Some("wayland".into()),
            desktop: Some("GNOME".into()),
            port: 4242,
            proto_version: 1,
            capabilities: 0,
            clipboard_backend: Some("portal".into()),
            clipboard_enabled: true,
        }));
        json_roundtrip(FrontendEvent::State(
            1,
            ClientConfig::default(),
            ClientState::default(),
        ));
        json_roundtrip(FrontendEvent::ClipboardStatus(ClipboardStatus {
            enabled: true,
            backend: Some("portal".into()),
            available: true,
            applied: 3,
            last_error: None,
        }));
    }

    #[test]
    fn frontend_requests_roundtrip() {
        json_roundtrip(FrontendRequest::Activate(1, true));
        json_roundtrip(FrontendRequest::Create);
        json_roundtrip(FrontendRequest::Delete(2));
        json_roundtrip(FrontendRequest::UpdatePosition(3, Position::Bottom));
        json_roundtrip(FrontendRequest::UpdateFixIps(
            4,
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        ));
        json_roundtrip(FrontendRequest::AuthorizeKey("pc".into(), "fp".into()));
        json_roundtrip(FrontendRequest::EnableCapture);
        json_roundtrip(FrontendRequest::SaveConfiguration);
        json_roundtrip(FrontendRequest::SetClipboardEnabled(true));
    }

    #[test]
    fn clipboard_status_summary_covers_every_state() {
        assert_eq!(ClipboardStatus::default().summary(), "disabled");
        let unavailable = ClipboardStatus {
            enabled: true,
            ..Default::default()
        };
        assert_eq!(unavailable.summary(), "backend unavailable");
        let ok = ClipboardStatus {
            enabled: true,
            available: true,
            backend: Some("portal".into()),
            applied: 7,
            ..Default::default()
        };
        assert_eq!(ok.summary(), "portal · 7 synced");
        let failed = ClipboardStatus {
            last_error: Some("permission denied".into()),
            ..ok
        };
        assert!(failed.summary().contains("permission denied"));
    }

    #[test]
    fn diagnostics_default_is_serializable() {
        let decoded = json_roundtrip(Diagnostics::default());
        assert_eq!(decoded.port, 0);
        assert!(decoded.capture_backend.is_none());
    }

    #[test]
    fn client_config_defaults() {
        let config = ClientConfig::default();
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.pos, Position::Left);
        assert!(config.hostname.is_none());
        assert!(config.fix_ips.is_empty());
    }

    #[test]
    fn default_port_matches_the_documented_value() {
        assert_eq!(DEFAULT_PORT, 4242);
    }
}

#[cfg(all(test, unix))]
mod ipc_tests {
    use super::*;
    use futures::StreamExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn socket_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("lan-mouse-ipc-tests");
        std::fs::create_dir_all(&dir).expect("test dir");
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        dir.join(format!("{}-{id}-{name}.sock", std::process::id()))
    }

    #[tokio::test]
    async fn service_and_frontend_exchange_json_lines() {
        let path = socket_path("roundtrip");
        let mut listener = AsyncFrontendListener::new_at(path.clone())
            .await
            .expect("listener");
        let (mut reader, mut writer) = connect_async_at(&path, Some(Duration::from_secs(2)))
            .await
            .expect("connect");

        writer
            .request(FrontendRequest::Enumerate())
            .await
            .expect("request");

        let mut saw_enumerate = false;
        for _ in 0..5 {
            let request = tokio::time::timeout(Duration::from_secs(5), listener.next())
                .await
                .expect("timed out waiting for request")
                .expect("listener closed");
            // The listener emits a synthetic `Sync` when a frontend connects.
            if matches!(request, Ok(FrontendRequest::Enumerate())) {
                saw_enumerate = true;
                break;
            }
        }
        assert!(saw_enumerate, "the request never reached the service");

        listener
            .broadcast(FrontendEvent::PortChanged(4242, None))
            .await;
        let event = tokio::time::timeout(Duration::from_secs(5), reader.next())
            .await
            .expect("timed out waiting for event")
            .expect("reader closed");
        assert!(matches!(event, Ok(FrontendEvent::PortChanged(4242, None))));

        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_second_listener_on_the_same_path_is_rejected() {
        let path = socket_path("duplicate");
        let first = AsyncFrontendListener::new_at(path.clone())
            .await
            .expect("first listener");
        let second = AsyncFrontendListener::new_at(path.clone()).await;
        assert!(matches!(
            second,
            Err(IpcListenerCreationError::AlreadyRunning)
        ));
        drop(first);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn connecting_to_a_missing_socket_times_out() {
        let path = socket_path("missing");
        let result = connect_async_at(&path, Some(Duration::from_millis(200))).await;
        assert!(matches!(result, Err(ConnectionError::Timeout)));
    }

    #[tokio::test]
    async fn stale_socket_file_is_replaced() {
        let path = socket_path("stale");
        // Simulate a crash that left the socket file behind.
        std::fs::write(&path, b"").expect("stale socket file");
        let listener = AsyncFrontendListener::new_at(path.clone()).await;
        assert!(listener.is_ok(), "stale socket must be cleaned up");
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }
}
