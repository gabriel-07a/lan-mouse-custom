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

/// Version of the local frontend IPC protocol.
///
/// The wire format is line-delimited JSON; adding enum variants is
/// forward-compatible because readers skip frames they cannot decode. The
/// handshake lets the service and the frontend advertise what they speak for
/// future feature gating.
pub const IPC_VERSION: u16 = 1;

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
    /// motion scale in percent (100 = unchanged). Wayland captures raw
    /// deltas while Windows/macOS report accelerated ones, so the speed
    /// differs between peer combinations and needs a per-client trim.
    #[serde(default = "default_sensitivity")]
    pub sensitivity: u16,
    /// SHA-256 fingerprint of the peer's certificate, when known.
    ///
    /// This is the identity of the peer, independent of its address. When it
    /// is set, outgoing connections verify the server's certificate against
    /// it (TOFU pinning); when it is `None` the first successful connection
    /// learns and stores it.
    #[serde(default)]
    pub fingerprint: Option<String>,
}

pub fn default_sensitivity() -> u16 {
    100
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
            sensitivity: default_sensitivity(),
            fingerprint: None,
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
    /// whether [`Self::peer_capabilities`] was actually reported by the peer
    /// (a legacy peer never reports anything, which is not the same as
    /// "not asked yet").
    #[serde(default)]
    pub peer_caps_known: bool,
    /// address of an active incoming connection from this peer, if any
    #[serde(default)]
    pub incoming_addr: Option<SocketAddr>,
    /// capabilities the peer advertised it grants to this machine
    /// (`incoming` on the peer side); only present on peers that speak the
    /// permission negotiation extension
    #[serde(default)]
    pub peer_incoming_caps: Option<u8>,
    /// capabilities the peer expects to use on this machine (`outgoing` on
    /// the peer side)
    #[serde(default)]
    pub peer_outgoing_caps: Option<u8>,
}

/// Bitset of input capabilities granted in one direction.
///
/// The three capabilities can be granted independently, so a device may e.g.
/// only move the pointer and use the clipboard but never type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities(pub u8);

impl Capabilities {
    pub const NONE: Self = Self(0);
    pub const POINTER: Self = Self(1 << 0);
    pub const KEYBOARD: Self = Self(1 << 1);
    pub const CLIPBOARD: Self = Self(1 << 2);
    pub const ALL: Self = Self(1 | 2 | 4);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Parse a comma separated list such as `pointer,clipboard`.
    pub fn from_names(names: &str) -> Option<Self> {
        let mut caps = Self::NONE;
        for name in names.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            caps = match name {
                "pointer" | "mouse" => Self(caps.0 | Self::POINTER.0),
                "keyboard" | "keys" => Self(caps.0 | Self::KEYBOARD.0),
                "clipboard" => Self(caps.0 | Self::CLIPBOARD.0),
                "all" => Self::ALL,
                "none" => Self::NONE,
                _ => return None,
            };
        }
        Some(caps)
    }

    /// Human readable capability names, in stable order.
    pub fn names(self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.contains(Self::POINTER) {
            names.push("pointer");
        }
        if self.contains(Self::KEYBOARD) {
            names.push("keyboard");
        }
        if self.contains(Self::CLIPBOARD) {
            names.push("clipboard");
        }
        names
    }

    /// Compact summary for logs and CLI output.
    pub fn summary(self) -> String {
        if self.is_empty() {
            "none".to_string()
        } else {
            self.names().join(",")
        }
    }
}

/// Permission granted in one direction between two devices.
///
/// `incoming` is authoritative and enforced by the machine that receives the
/// input; `outgoing` is a local guard that decides whether this machine may
/// control the peer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectionPolicy {
    /// whether this direction is allowed at all
    #[serde(default)]
    pub allowed: bool,
    /// capabilities granted when `allowed`
    #[serde(default)]
    pub capabilities: Capabilities,
}

impl DirectionPolicy {
    pub fn allow_all() -> Self {
        Self {
            allowed: true,
            capabilities: Capabilities::ALL,
        }
    }

    pub fn deny() -> Self {
        Self {
            allowed: false,
            capabilities: Capabilities::NONE,
        }
    }

    pub fn allows(&self, capability: Capabilities) -> bool {
        self.allowed && self.capabilities.contains(capability)
    }

    /// Whether the direction is enabled with at least one capability.
    ///
    /// A direction with `allowed = true` but no capabilities is connect-only:
    /// the peer can stay connected but cannot do anything, so no link needs to
    /// be activated for it.
    pub fn grants_anything(&self) -> bool {
        self.allowed && !self.capabilities.is_empty()
    }

    /// Whether any input capability (pointer or keyboard) is granted.
    pub fn allows_input(&self) -> bool {
        self.allowed
            && (self.capabilities.intersects(Capabilities::POINTER)
                || self.capabilities.intersects(Capabilities::KEYBOARD))
    }
}

/// One device known to this machine, as shown to the frontend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceView {
    /// stable identity: SHA-256 fingerprint of the peer certificate
    pub fingerprint: String,
    /// user visible name (renameable)
    pub name: String,
    /// where the device sits relative to this one
    pub position: Position,
    /// route hints (outgoing connections)
    pub addresses: Vec<IpAddr>,
    pub port: u16,
    /// what the device may do here (enforced locally)
    pub incoming: DirectionPolicy,
    /// what this machine may do there (local guard)
    pub outgoing: DirectionPolicy,
    /// outgoing link is activated
    pub active: bool,
    /// a session (incoming or outgoing) is live
    pub connected: bool,
    /// how the device was authorized (`manual`, `pairing`, `address`, `legacy`)
    pub source: String,
}

/// One persisted audit event, newest first in history snapshots.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// unix timestamp in seconds
    pub ts: u64,
    /// machine readable event name (`grant_created`, `revoke`, ...)
    pub event: String,
    #[serde(default)]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub addr: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

/// An address range that auto-authorizes connecting devices with fixed
/// permissions.
///
/// Replaces the old `trusted_ips` list, which always granted full access:
/// every device adopted through a rule now gets exactly the capabilities the
/// rule carries, and the user can still narrow or revoke it per device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressRuleView {
    /// `192.168.1.50` or `10.0.0.0/24`
    pub address: String,
    /// permissions the adopted device gets in the incoming direction
    pub incoming: DirectionPolicy,
    /// permissions the adopted device gets in the outgoing direction
    pub outgoing: DirectionPolicy,
    /// the range authorizes more than a typical local subnet
    pub broad: bool,
}

/// A device asking to connect that has not been authorized yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingView {
    pub fingerprint: String,
    pub addr: SocketAddr,
    /// name reported by discovery, when known
    pub name: Option<String>,
    /// six-digit comparison code derived from both fingerprints
    #[serde(default)]
    pub sas: String,
    /// fingerprint previously known for the same address/name, when this
    /// request looks like a certificate change
    #[serde(default)]
    pub previous_fingerprint: Option<String>,
    /// unix timestamp (seconds) of the first/last attempt
    pub first_seen: u64,
    pub last_seen: u64,
    pub attempts: u32,
}

/// How a device is reached on the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LinkKind {
    Wired,
    Wireless,
    #[default]
    Unknown,
}

impl LinkKind {
    /// Icon name used by the GTK frontend.
    pub fn icon_name(&self) -> &'static str {
        match self {
            LinkKind::Wired => "network-wired-symbolic",
            LinkKind::Wireless => "network-wireless-symbolic",
            LinkKind::Unknown => "network-offline-symbolic",
        }
    }
}

impl std::fmt::Display for LinkKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkKind::Wired => write!(f, "wired"),
            LinkKind::Wireless => write!(f, "wireless"),
            LinkKind::Unknown => write!(f, "unknown"),
        }
    }
}

/// One entry of the network table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    pub ip: std::net::IpAddr,
    /// Host name reported by the peer (our builds only).
    pub hostname: Option<String>,
    /// Listen port reported by the peer.
    pub port: Option<u16>,
    /// Build version reported by the peer.
    pub version: Option<String>,
    /// Wired/wireless, as reported by the peer or derived from our route.
    pub link: LinkKind,
    /// The host answered the discovery beacon (runs this fork).
    pub lan_mouse: bool,
    /// Already configured as a client of this machine.
    pub configured: bool,
    /// Certificate fingerprint reported by the peer (our builds only), so a
    /// device can be authorized before it ever connects.
    #[serde(default)]
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FrontendEvent {
    /// IPC version spoken by the service (reply to [`FrontendRequest::Hello`])
    Hello { ipc_version: u16 },
    /// full snapshot of the known devices and their policies
    Devices(Vec<DeviceView>),
    /// full snapshot of unauthorized connection requests
    PendingRequests(Vec<PendingView>),
    /// devices adopted during the pairing window that just closed, for review
    PairingReview(Vec<DeviceView>),
    /// recent audit events, newest first
    ConnectionHistory(Vec<HistoryEntry>),
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
    ConnectionAttempt {
        addr: SocketAddr,
        fingerprint: String,
        /// six-digit code to compare with the other machine
        #[serde(default)]
        sas: String,
        /// set when the same address previously presented another certificate
        #[serde(default)]
        previous_fingerprint: Option<String>,
    },
    /// a known device showed up with a different certificate than before
    CertificateChanged {
        old_fingerprint: String,
        new_fingerprint: String,
        name: String,
        addr: SocketAddr,
        /// six-digit comparison code for the new certificate
        #[serde(default)]
        sas: String,
    },
    /// runtime diagnostics (backends, protocol version, …)
    Diagnostics(Diagnostics),
    /// result of a network scan
    NetworkScan(Vec<DiscoveredDevice>),
    /// result of an explicit Lan Mouse probe
    ProbeResult {
        ip: std::net::IpAddr,
        lan_mouse: bool,
    },
    /// addresses allowed to connect without an authorization prompt
    TrustedAddresses(Vec<String>),
    /// structured view of the auto-authorizing address rules
    AddressRules(Vec<AddressRuleView>),
    /// whether the pairing window is currently open
    PairingActive(bool),
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
    /// number of local clipboard changes detected since startup. A zero here
    /// while copying means the backend is not observing the clipboard at all.
    #[serde(default)]
    pub detected: u64,
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
        let mut summary = format!(
            "{backend} · {} received · {} local",
            self.applied, self.detected
        );
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
    /// announce the frontend IPC version
    Hello { ipc_version: u16 },
    /// request the device snapshot
    ListDevices,
    /// request the pending-request snapshot
    ListPending,
    /// update the permissions of a known device
    SetDevicePolicy {
        fingerprint: String,
        incoming: DirectionPolicy,
        outgoing: DirectionPolicy,
    },
    /// rename a device (display only; the fingerprint stays the identity)
    RenameDevice { fingerprint: String, name: String },
    /// revoke a device: close live sessions, deny the fingerprint and drop it
    RevokeDevice {
        fingerprint: String,
        /// close sessions that are currently live
        disconnect: bool,
    },
    /// approve or deny a pending connection request
    RespondPending {
        fingerprint: String,
        accept: bool,
        name: Option<String>,
        position: Option<Position>,
        incoming: Option<DirectionPolicy>,
        outgoing: Option<DirectionPolicy>,
    },
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
    /// update the motion scale of a client, in percent
    UpdateSensitivity(ClientHandle, u16),
    /// update fix-ips
    UpdateFixIps(ClientHandle, Vec<IpAddr>),
    /// pin (or clear) the expected certificate fingerprint of a client
    UpdateFingerprint(ClientHandle, Option<String>),
    /// request reenabling input capture
    EnableCapture,
    /// request reenabling input emulation
    EnableEmulation,
    /// synchronize all state
    Sync,
    /// authorize fingerprint (description, fingerprint)
    AuthorizeKey(String, String),
    /// Authorize a device and configure the reverse direction in one step.
    ///
    /// `position` is where the peer sits relative to this device. The service
    /// stores the fingerprint and, as soon as the peer's connection is
    /// accepted (which yields its address), creates and activates a client
    /// entry for it, so both machines can control each other without editing
    /// the config by hand.
    AuthorizeDevice {
        fingerprint: String,
        name: String,
        position: Position,
    },
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
    /// rescan the local network
    ScanNetwork,
    /// explicitly test a host for a Lan Mouse listener
    ProbeDevice { ip: std::net::IpAddr, port: u16 },
    /// allow connections from this address (IP or CIDR) without a prompt,
    /// granting full access (legacy)
    TrustAddress(String),
    /// allow connections from this address with explicit permissions
    AddAddressRule {
        address: String,
        incoming: DirectionPolicy,
        outgoing: DirectionPolicy,
    },
    /// request the structured address-rule list
    ListAddressRules,
    /// request the most recent audit entries
    ListHistory { limit: u32 },
    /// remove an address from the allowlist
    UntrustAddress(String),
    /// accept any device that connects during the next `seconds`
    StartPairing(u64),
    /// accept one device during the next `seconds`, granting these permissions
    StartPairingScoped {
        seconds: u64,
        incoming: DirectionPolicy,
        outgoing: DirectionPolicy,
    },
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
            peer_caps_known: true,
            incoming_addr: Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
                51234,
            )),
            peer_incoming_caps: Some(0b101),
            peer_outgoing_caps: Some(0b011),
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
            addr: "192.168.1.5:4242".parse().unwrap(),
            fingerprint: "aa:bb".into(),
            sas: "123456".into(),
            previous_fingerprint: Some("cc:dd".into()),
        });
        json_roundtrip(FrontendEvent::CertificateChanged {
            old_fingerprint: "cc:dd".into(),
            new_fingerprint: "aa:bb".into(),
            name: "laptop".into(),
            addr: "192.168.1.5:4242".parse().unwrap(),
            sas: "123456".into(),
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
            detected: 4,
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
        json_roundtrip(FrontendRequest::AuthorizeDevice {
            fingerprint: "fp".into(),
            name: "pc".into(),
            position: Position::Right,
        });
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
        assert_eq!(ok.summary(), "portal · 7 received · 0 local");
        let both = ClipboardStatus {
            detected: 3,
            ..ok.clone()
        };
        assert_eq!(both.summary(), "portal · 7 received · 3 local");
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
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use std::net::Ipv4Addr;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    fn json_roundtrip<T>(value: T) -> T
    where
        T: Serialize + DeserializeOwned,
    {
        let encoded = serde_json::to_value(&value).expect("serialize");
        let decoded: T = serde_json::from_value(encoded.clone()).expect("deserialize");
        let re_encoded = serde_json::to_value(&decoded).expect("re-serialize");
        assert_eq!(encoded, re_encoded);
        decoded
    }

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

    #[test]
    fn capabilities_parse_and_summarise() {
        assert_eq!(
            Capabilities::from_names("pointer,keyboard").expect("parse"),
            Capabilities(Capabilities::POINTER.0 | Capabilities::KEYBOARD.0)
        );
        assert_eq!(
            Capabilities::from_names("mouse, keys, clipboard").expect("parse"),
            Capabilities::ALL
        );
        assert_eq!(Capabilities::from_names("nonsense"), None);
        assert_eq!(
            Capabilities::ALL.names(),
            vec!["pointer", "keyboard", "clipboard"]
        );
        assert_eq!(Capabilities::NONE.summary(), "none");
        assert!(Capabilities::ALL.contains(Capabilities::CLIPBOARD));
        assert!(!Capabilities::NONE.intersects(Capabilities::POINTER));
    }

    #[test]
    fn direction_policy_serialisation_roundtrips() {
        let policy = DirectionPolicy {
            allowed: true,
            capabilities: Capabilities(Capabilities::POINTER.0 | Capabilities::CLIPBOARD.0),
        };
        let decoded = json_roundtrip(policy);
        assert_eq!(decoded, policy);
        assert!(decoded.allows(Capabilities::POINTER));
        assert!(!decoded.allows(Capabilities::KEYBOARD));
        assert!(decoded.allows_input());
        assert!(!DirectionPolicy::deny().allows_input());
        assert!(decoded.grants_anything());
        assert!(DirectionPolicy::allow_all().grants_anything());
        assert!(!DirectionPolicy::deny().grants_anything());
        assert!(
            !DirectionPolicy {
                allowed: true,
                capabilities: Capabilities::NONE,
            }
            .grants_anything(),
            "connect-only directions do not need a link"
        );
        assert!(!DirectionPolicy::default().allowed, "default is deny");
    }

    #[test]
    fn device_and_pending_views_roundtrip() {
        let device = DeviceView {
            fingerprint: "aa:bb".into(),
            name: "laptop".into(),
            position: Position::Right,
            addresses: vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))],
            port: DEFAULT_PORT,
            incoming: DirectionPolicy {
                allowed: true,
                capabilities: Capabilities::KEYBOARD,
            },
            outgoing: DirectionPolicy::deny(),
            active: true,
            connected: true,
            source: "manual".into(),
        };
        assert_eq!(json_roundtrip(device.clone()), device);

        let pending = PendingView {
            fingerprint: "cc:dd".into(),
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)), 4242),
            name: Some("desktop".into()),
            sas: "654321".into(),
            previous_fingerprint: Some("ee:ff".into()),
            first_seen: 1,
            last_seen: 2,
            attempts: 3,
        };
        assert_eq!(json_roundtrip(pending.clone()), pending);
    }

    #[test]
    fn history_entries_and_requests_roundtrip() {
        let entry = HistoryEntry {
            ts: 1_790_000_000,
            event: "grant_created".into(),
            fingerprint: Some("aa:bb".into()),
            name: Some("laptop".into()),
            addr: Some("10.0.0.2:4242".into()),
            detail: Some("source=manual incoming=[pointer]".into()),
        };
        assert_eq!(json_roundtrip(entry.clone()), entry);
        let request = FrontendRequest::ListHistory { limit: 25 };
        assert_eq!(json_roundtrip(request.clone()), request);
    }

    #[test]
    fn address_rule_views_and_requests_roundtrip() {
        let rule = AddressRuleView {
            address: "10.0.0.0/24".into(),
            incoming: DirectionPolicy {
                allowed: true,
                capabilities: Capabilities::CLIPBOARD,
            },
            outgoing: DirectionPolicy::deny(),
            broad: true,
        };
        assert_eq!(json_roundtrip(rule.clone()), rule);

        let requests = vec![
            FrontendRequest::AddAddressRule {
                address: "10.0.0.0/24".into(),
                incoming: DirectionPolicy::allow_all(),
                outgoing: DirectionPolicy::deny(),
            },
            FrontendRequest::ListAddressRules,
        ];
        for request in requests {
            assert_eq!(json_roundtrip(request.clone()), request);
        }
    }

    #[test]
    fn permission_requests_roundtrip() {
        let requests = vec![
            FrontendRequest::ListDevices,
            FrontendRequest::ListPending,
            FrontendRequest::SetDevicePolicy {
                fingerprint: "aa".into(),
                incoming: DirectionPolicy::allow_all(),
                outgoing: DirectionPolicy::deny(),
            },
            FrontendRequest::RenameDevice {
                fingerprint: "aa".into(),
                name: "new name".into(),
            },
            FrontendRequest::RevokeDevice {
                fingerprint: "aa".into(),
                disconnect: true,
            },
            FrontendRequest::RespondPending {
                fingerprint: "bb".into(),
                accept: true,
                name: Some("peer".into()),
                position: Some(Position::Bottom),
                incoming: Some(DirectionPolicy::allow_all()),
                outgoing: Some(DirectionPolicy::deny()),
            },
        ];
        for request in requests {
            assert_eq!(json_roundtrip(request.clone()), request);
        }
    }
}
