use crate::capture_test::TestCaptureArgs;
use crate::doctor::DoctorArgs;
use crate::emulation_test::TestEmulationArgs;
use clap::{Parser, Subcommand, ValueEnum};
use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env::{self, VarError};
use std::fmt::Display;
use std::fs::{self, File};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::{collections::HashSet, io};
use thiserror::Error;
use toml;
use toml_edit::{self, DocumentMut};

use lan_mouse_cli::CliArgs;
use lan_mouse_ipc::{DEFAULT_PORT, Position};

use input_event::scancode::{
    self,
    Linux::{KeyLeftAlt, KeyLeftCtrl, KeyLeftMeta, KeyLeftShift},
};

use shadow_rs::shadow;

shadow!(build);

/// Local build's 8-byte ASCII short commit hash, suitable for use
/// in [`lan_mouse_proto::ProtoEvent::Hello`]. Pads with `'?'` if
/// shadow_rs returns an unexpected length so the field is always
/// well-formed on the wire.
pub fn local_commit() -> [u8; 8] {
    let bytes = build::SHORT_COMMIT.as_bytes();
    let mut out = [b'?'; 8];
    let n = bytes.len().min(8);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

const CONFIG_FILE_NAME: &str = "config.toml";
const CERT_FILE_NAME: &str = "lan-mouse.pem";

fn default_path() -> Result<PathBuf, VarError> {
    #[cfg(unix)]
    let default_path = {
        let xdg_config_home =
            env::var("XDG_CONFIG_HOME").unwrap_or(format!("{}/.config", env::var("HOME")?));
        format!("{xdg_config_home}/lan-mouse/")
    };

    #[cfg(not(unix))]
    let default_path = {
        let app_data =
            env::var("LOCALAPPDATA").unwrap_or(format!("{}/.config", env::var("USERPROFILE")?));
        format!("{app_data}\\lan-mouse\\")
    };
    Ok(PathBuf::from(default_path))
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct ConfigToml {
    capture_backend: Option<CaptureBackend>,
    emulation_backend: Option<EmulationBackend>,
    port: Option<u16>,
    release_bind: Option<Vec<scancode::Linux>>,
    /// batch window for pointer motion events in milliseconds (0 disables)
    motion_batch_ms: Option<u64>,
    /// how long a send may wait for a (re)connection before giving up, in ms
    connect_grace_ms: Option<u64>,
    /// clipboard synchronization settings
    clipboard: Option<ClipboardConfig>,
    cert_path: Option<PathBuf>,
    clients: Option<Vec<TomlClient>>,
    authorized_fingerprints: Option<HashMap<String, String>>,
    /// addresses that may connect without an authorization prompt
    trusted_ips: Option<Vec<String>>,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default, Eq, PartialEq)]
struct ClipboardConfig {
    enabled: Option<bool>,
    max_text_bytes: Option<usize>,
    max_html_bytes: Option<usize>,
    max_image_bytes: Option<usize>,
    max_snapshot_bytes: Option<usize>,
}

impl ClipboardConfig {
    fn limits(&self) -> input_clipboard::Limits {
        let default = input_clipboard::Limits::default();
        input_clipboard::Limits {
            max_text: self.max_text_bytes.unwrap_or(default.max_text),
            max_html: self.max_html_bytes.unwrap_or(default.max_html),
            max_image: self.max_image_bytes.unwrap_or(default.max_image),
            max_snapshot: self.max_snapshot_bytes.unwrap_or(default.max_snapshot),
        }
    }

    fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
struct TomlClient {
    hostname: Option<String>,
    host_name: Option<String>,
    ips: Option<Vec<IpAddr>>,
    port: Option<u16>,
    position: Option<Position>,
    activate_on_startup: Option<bool>,
    enter_hook: Option<String>,
    leave_hook: Option<String>,
    /// motion scale in percent (100 = unchanged)
    sensitivity: Option<u16>,
}

impl ConfigToml {
    fn new(path: &Path) -> Result<ConfigToml, ConfigError> {
        let config = fs::read_to_string(path)?;
        Ok(toml::from_str::<_>(&config)?)
    }
}

#[derive(Parser, Debug)]
#[command(author, version=build::CLAP_LONG_VERSION, about, long_about = None)]
struct Args {
    /// the listen port for lan-mouse
    #[arg(short, long)]
    port: Option<u16>,

    /// non-default config file location
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// batch window for pointer motion events in milliseconds (0 disables)
    #[arg(long)]
    motion_batch_ms: Option<u64>,

    /// how long an input event may wait for a (re)connection before the
    /// capture is released, in milliseconds (0 disables the grace period)
    #[arg(long)]
    connect_grace_ms: Option<u64>,

    /// disable clipboard synchronization
    #[arg(long)]
    no_clipboard: bool,

    /// capture backend override
    #[arg(long)]
    capture_backend: Option<CaptureBackend>,

    /// emulation backend override
    #[arg(long)]
    emulation_backend: Option<EmulationBackend>,

    /// path to non-default certificate location
    #[arg(long)]
    cert_path: Option<PathBuf>,

    /// subcommands
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// test input emulation
    TestEmulation(TestEmulationArgs),
    /// test input capture
    TestCapture(TestCaptureArgs),
    /// Lan Mouse commandline interface
    Cli(CliArgs),
    /// diagnose input backends, config and connectivity
    Doctor(DoctorArgs),
    /// run in daemon mode
    Daemon,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
pub enum CaptureBackend {
    #[cfg(libei_capture)]
    #[serde(rename = "input-capture-portal")]
    InputCapturePortal,
    #[cfg(layer_shell_capture)]
    #[serde(rename = "layer-shell")]
    LayerShell,
    #[cfg(x11_capture)]
    #[serde(rename = "x11")]
    X11,
    #[cfg(windows)]
    #[serde(rename = "windows")]
    Windows,
    #[cfg(target_os = "macos")]
    #[serde(rename = "macos")]
    MacOs,
    #[serde(rename = "dummy")]
    Dummy,
}

impl Display for CaptureBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(libei_capture)]
            CaptureBackend::InputCapturePortal => write!(f, "input-capture-portal"),
            #[cfg(layer_shell_capture)]
            CaptureBackend::LayerShell => write!(f, "layer-shell"),
            #[cfg(x11_capture)]
            CaptureBackend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            CaptureBackend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            CaptureBackend::MacOs => write!(f, "MacOS"),
            CaptureBackend::Dummy => write!(f, "dummy"),
        }
    }
}

impl From<CaptureBackend> for input_capture::Backend {
    fn from(backend: CaptureBackend) -> Self {
        match backend {
            #[cfg(libei_capture)]
            CaptureBackend::InputCapturePortal => Self::InputCapturePortal,
            #[cfg(layer_shell_capture)]
            CaptureBackend::LayerShell => Self::LayerShell,
            #[cfg(x11_capture)]
            CaptureBackend::X11 => Self::X11,
            #[cfg(windows)]
            CaptureBackend::Windows => Self::Windows,
            #[cfg(target_os = "macos")]
            CaptureBackend::MacOs => Self::MacOs,
            CaptureBackend::Dummy => Self::Dummy,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
pub enum EmulationBackend {
    #[cfg(wlroots_emulation)]
    #[serde(rename = "wlroots")]
    Wlroots,
    #[cfg(libei_emulation)]
    #[serde(rename = "libei")]
    Libei,
    #[cfg(rdp_emulation)]
    #[serde(rename = "xdp")]
    Xdp,
    #[cfg(x11_emulation)]
    #[serde(rename = "x11")]
    X11,
    #[cfg(windows)]
    #[serde(rename = "windows")]
    Windows,
    #[cfg(target_os = "macos")]
    #[serde(rename = "macos")]
    MacOs,
    #[serde(rename = "dummy")]
    Dummy,
}

impl From<EmulationBackend> for input_emulation::Backend {
    fn from(backend: EmulationBackend) -> Self {
        match backend {
            #[cfg(wlroots_emulation)]
            EmulationBackend::Wlroots => Self::Wlroots,
            #[cfg(libei_emulation)]
            EmulationBackend::Libei => Self::Libei,
            #[cfg(rdp_emulation)]
            EmulationBackend::Xdp => Self::Xdp,
            #[cfg(x11_emulation)]
            EmulationBackend::X11 => Self::X11,
            #[cfg(windows)]
            EmulationBackend::Windows => Self::Windows,
            #[cfg(target_os = "macos")]
            EmulationBackend::MacOs => Self::MacOs,
            EmulationBackend::Dummy => Self::Dummy,
        }
    }
}

impl Display for EmulationBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(wlroots_emulation)]
            EmulationBackend::Wlroots => write!(f, "wlroots"),
            #[cfg(libei_emulation)]
            EmulationBackend::Libei => write!(f, "libei"),
            #[cfg(rdp_emulation)]
            EmulationBackend::Xdp => write!(f, "xdg-desktop-portal"),
            #[cfg(x11_emulation)]
            EmulationBackend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            EmulationBackend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            EmulationBackend::MacOs => write!(f, "macos"),
            EmulationBackend::Dummy => write!(f, "dummy"),
        }
    }
}

#[derive(Debug)]
pub struct Config {
    /// command line arguments
    args: Args,
    /// path to the certificate file used
    cert_path: PathBuf,
    /// path to the config file used
    config_path: PathBuf,
    /// path to config directory (parent of above)
    config_dir: PathBuf,
    /// the (optional) toml config and it's path
    config_toml: Option<ConfigToml>,
    // filesystem watcher
    watcher: notify::RecommendedWatcher,
    // channel for filesystem events
    watch_rx: tokio::sync::mpsc::Receiver<Result<notify::Event, notify::Error>>,
}

pub struct ConfigClient {
    pub ips: HashSet<IpAddr>,
    pub hostname: Option<String>,
    pub port: u16,
    pub pos: Position,
    pub active: bool,
    pub enter_hook: Option<String>,
    pub leave_hook: Option<String>,
    /// motion scale in percent (100 = unchanged)
    pub sensitivity: u16,
}

impl From<TomlClient> for ConfigClient {
    fn from(toml: TomlClient) -> Self {
        let active = toml.activate_on_startup.unwrap_or(false);
        let enter_hook = toml.enter_hook;
        let leave_hook = toml.leave_hook;
        let sensitivity = toml.sensitivity.unwrap_or(100).clamp(10, 500);
        let hostname = toml.hostname;
        let ips = HashSet::from_iter(toml.ips.into_iter().flatten());
        let port = toml.port.unwrap_or(DEFAULT_PORT);
        let pos = toml.position.unwrap_or_default();
        Self {
            ips,
            hostname,
            port,
            pos,
            active,
            enter_hook,
            leave_hook,
            sensitivity,
        }
    }
}

impl From<ConfigClient> for TomlClient {
    fn from(client: ConfigClient) -> Self {
        let hostname = client.hostname;
        let host_name = None;
        let mut ips = client.ips.into_iter().collect::<Vec<_>>();
        ips.sort();
        let ips = Some(ips);
        let port = if client.port == DEFAULT_PORT {
            None
        } else {
            Some(client.port)
        };
        let position = Some(client.pos);
        let activate_on_startup = if client.active { Some(true) } else { None };
        let enter_hook = client.enter_hook;
        let leave_hook = client.leave_hook;
        let sensitivity = if client.sensitivity == 100 {
            None
        } else {
            Some(client.sensitivity)
        };
        Self {
            hostname,
            host_name,
            ips,
            port,
            position,
            activate_on_startup,
            enter_hook,
            leave_hook,
            sensitivity,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Var(#[from] VarError),
    #[error(transparent)]
    Watcher(#[from] notify::Error),
}

const DEFAULT_RELEASE_KEYS: [scancode::Linux; 4] =
    [KeyLeftCtrl, KeyLeftShift, KeyLeftMeta, KeyLeftAlt];

/// Default batching window for pointer motion events. 2 ms caps the
/// motion packet rate at ~500/s while staying below the latency of a
/// typical wireless mouse. `0` disables coalescing.
const DEFAULT_MOTION_BATCH_MS: u64 = 2;

/// How long a single input event may block on a broken connection before
/// the capture is released. Keeps the grab during short Wi-Fi hiccups
/// instead of bouncing the cursor back to the local screen immediately.
const DEFAULT_CONNECT_GRACE_MS: u64 = 400;

impl Config {
    pub fn new() -> Result<Self, ConfigError> {
        let args = Args::parse();

        // --config <file> overrules default location
        let config_path = args
            .config
            .clone()
            .unwrap_or(default_path()?.join(CONFIG_FILE_NAME));
        let config_dir = config_path
            .parent()
            .expect("config directory")
            .to_path_buf();

        // Ensure the config directory exists and write a default config file
        // if none is present. Runs on every Config::new(), regardless of which
        // entry path (GUI main, spawned daemon, CLI, test commands) we're on,
        // so a fresh Mac never hits "No such file or directory" on config.toml
        // and notify::Watcher (which requires the dir to exist on macOS
        // FSEvents and some Linux backends) has a concrete path to watch.
        fs::create_dir_all(&config_dir)?;
        if !config_path.exists() {
            let default_toml = toml::to_string_pretty(&ConfigToml::default())
                .expect("default ConfigToml serialization cannot fail");
            fs::write(&config_path, default_toml)?;
        }

        let config_toml = match ConfigToml::new(&config_path) {
            Err(e) => {
                log::warn!("{config_path:?}: {e}");
                log::warn!("Continuing without config file ...");
                None
            }
            Ok(c) => Some(c),
        };

        // --cert-path <file> overrules default location
        let cert_path = args
            .cert_path
            .clone()
            .or(config_toml.as_ref().and_then(|c| c.cert_path.clone()))
            .unwrap_or(default_path()?.join(CERT_FILE_NAME));

        let (tx, watch_rx) = tokio::sync::mpsc::channel(16);
        let watcher = RecommendedWatcher::new(
            move |res| {
                let _ = tx.blocking_send(res);
            },
            notify::Config::default(),
        )?;
        let mut config = Config {
            args,
            cert_path,
            config_path,
            config_dir,
            config_toml,
            watcher,
            watch_rx,
        };
        config.watch()?;
        Ok(config)
    }

    fn watch(&mut self) -> Result<(), notify::Error> {
        self.watcher
            .watch(&self.config_dir, notify::RecursiveMode::NonRecursive)?;
        Ok(())
    }

    fn unwatch(&mut self) -> Result<(), notify::Error> {
        self.watcher.unwatch(&self.config_dir)?;
        Ok(())
    }

    pub async fn changed(&mut self) -> Result<(), notify::Error> {
        loop {
            let event = self.watch_rx.recv().await.expect("channel closed");
            let event = match event {
                Ok(event) => event,
                Err(e) => {
                    log::warn!("config watcher error: {e}");
                    continue;
                }
            };
            if event.paths.contains(&self.config_path)
                && matches!(
                    event.kind,
                    EventKind::Create(_)
                        | EventKind::Modify(ModifyKind::Data(_))
                        | EventKind::Remove(_)
                )
                && self.read_from_disk()?
            {
                return Ok(());
            }
        }
    }

    /// the command to run
    pub fn command(&self) -> Option<Command> {
        self.args.command.clone()
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// public key fingerprints authorized for connection
    /// addresses (IP or CIDR) allowed to connect without a prompt
    pub fn trusted_ips(&self) -> Vec<String> {
        self.config_toml
            .as_ref()
            .and_then(|c| c.trusted_ips.clone())
            .unwrap_or_default()
    }

    pub fn authorized_fingerprints(&self) -> HashMap<String, String> {
        self.config_toml
            .as_ref()
            .and_then(|c| c.authorized_fingerprints.clone())
            .unwrap_or_default()
    }

    /// path to certificate
    pub fn cert_path(&self) -> &Path {
        &self.cert_path
    }

    /// optional input-capture backend override
    pub fn capture_backend(&self) -> Option<CaptureBackend> {
        self.args
            .capture_backend
            .or(self.config_toml.as_ref().and_then(|c| c.capture_backend))
    }

    /// optional input-emulation backend override
    pub fn emulation_backend(&self) -> Option<EmulationBackend> {
        self.args
            .emulation_backend
            .or(self.config_toml.as_ref().and_then(|c| c.emulation_backend))
    }

    /// the port to use (initially)
    pub fn port(&self) -> u16 {
        self.args
            .port
            .or(self.config_toml.as_ref().and_then(|c| c.port))
            .unwrap_or(DEFAULT_PORT)
    }

    /// list of configured clients
    pub fn clients(&self) -> Vec<ConfigClient> {
        self.config_toml
            .as_ref()
            .map(|c| c.clients.clone())
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .map(From::<TomlClient>::from)
            .collect()
    }

    /// release bind for returning control to the host
    pub fn release_bind(&self) -> Vec<scancode::Linux> {
        self.config_toml
            .as_ref()
            .and_then(|c| c.release_bind.clone())
            .unwrap_or(Vec::from_iter(DEFAULT_RELEASE_KEYS.iter().cloned()))
    }

    /// batching window for pointer motion events (0 disables coalescing)
    pub fn motion_batch_ms(&self) -> u64 {
        self.args
            .motion_batch_ms
            .or(self.config_toml.as_ref().and_then(|c| c.motion_batch_ms))
            .unwrap_or(DEFAULT_MOTION_BATCH_MS)
    }

    /// how long a send may wait for a (re)connection before giving up
    pub fn connect_grace_ms(&self) -> u64 {
        self.args
            .connect_grace_ms
            .or(self.config_toml.as_ref().and_then(|c| c.connect_grace_ms))
            .unwrap_or(DEFAULT_CONNECT_GRACE_MS)
    }

    /// whether clipboard synchronization is enabled at startup
    pub fn clipboard_enabled(&self) -> bool {
        if self.args.no_clipboard {
            return false;
        }
        self.config_toml
            .as_ref()
            .and_then(|c| c.clipboard.as_ref())
            .is_none_or(ClipboardConfig::enabled)
    }

    /// persist the clipboard toggle in the config file
    pub fn set_clipboard_enabled(&mut self, enabled: bool) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        let config = self.config_toml.as_mut().expect("config");
        let clipboard = config.clipboard.get_or_insert_with(Default::default);
        clipboard.enabled = Some(enabled);
    }

    /// clipboard size limits
    pub fn clipboard_limits(&self) -> input_clipboard::Limits {
        self.config_toml
            .as_ref()
            .and_then(|c| c.clipboard.as_ref())
            .map(ClipboardConfig::limits)
            .unwrap_or_default()
    }

    /// set configured clients
    pub fn set_clients(&mut self, clients: Vec<ConfigClient>) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml.as_mut().expect("config").clients =
            Some(clients.into_iter().map(|c| c.into()).collect::<Vec<_>>());
    }

    /// set the trusted address allowlist
    pub fn set_trusted_ips(&mut self, trusted: Vec<String>) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml.as_mut().expect("config").trusted_ips = if trusted.is_empty() {
            None
        } else {
            Some(trusted)
        };
    }

    /// set authorized keys
    pub fn set_authorized_keys(&mut self, fingerprints: HashMap<String, String>) {
        if self.config_toml.is_none() {
            self.config_toml = Some(Default::default());
        }
        self.config_toml
            .as_mut()
            .expect("config")
            .authorized_fingerprints = Some(fingerprints);
    }

    pub fn read_from_disk(&mut self) -> Result<bool, io::Error> {
        log::info!("reading config from {:?}", self.config_path);

        let current_config = fs::read_to_string(&self.config_path)?;
        let current_config = match current_config.parse::<DocumentMut>() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("{:?} {e}", self.config_path());
                return Ok(false);
            }
        };
        let mut changed = false;
        match toml_edit::de::from_document::<ConfigToml>(current_config) {
            Ok(current_config) => {
                changed = self
                    .config_toml
                    .as_ref()
                    .is_none_or(|c| c != &current_config);
                self.config_toml.replace(current_config);
            }
            Err(e) => log::warn!("{:?} {e}", self.config_path()),
        };
        if changed {
            log::info!("config changed");
        } else {
            log::info!("config unchanged");
        }
        Ok(changed)
    }

    pub fn write_back(&mut self) -> Result<(), io::Error> {
        log::info!("writing config to {:?}", self.config_path);
        /* the new config */
        let new_config = self.config_toml.clone().unwrap_or_default();
        let new_config = toml_edit::ser::to_string_pretty(&new_config).expect("config");

        /*
         * TODO merge with current config file to preserve comments
         * => eventually we might want to split this up into clients configured
         * via the config file and clients managed through the GUI / frontend.
         * The latter should be saved to $XDG_DATA_HOME instead of $XDG_CONFIG_HOME,
         * and clients configured through .config could be made permanent.
         * For now we just override the config file.
         */

        let _ = self.unwatch();
        /* write new config to file */
        if let Some(p) = self.config_path().parent() {
            fs::create_dir_all(p)?;
        }
        {
            let mut f = File::create(self.config_path())?;
            f.write_all(new_config.as_bytes())?;
            f.sync_all()?;
        }

        let _ = self.watch();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> ConfigToml {
        toml::from_str(toml).expect("parse config")
    }

    fn toml_client() -> TomlClient {
        TomlClient {
            hostname: Some("peer".into()),
            host_name: None,
            ips: Some(vec![IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 2))]),
            port: None,
            position: None,
            activate_on_startup: None,
            enter_hook: None,
            leave_hook: None,
            sensitivity: None,
        }
    }

    #[test]
    fn shipped_example_config_parses() {
        let config: ConfigToml =
            toml::from_str(include_str!("../config.toml")).expect("example config must parse");
        assert_eq!(config.port, Some(4242));
        assert!(config.release_bind.is_some());
        assert_eq!(config.clients.as_ref().map(Vec::len), Some(2));
        assert_eq!(
            config.authorized_fingerprints.as_ref().map(HashMap::len),
            Some(1)
        );
    }

    #[test]
    fn empty_config_has_no_overrides() {
        let config = parse("");
        assert!(config.port.is_none());
        assert!(config.motion_batch_ms.is_none());
        assert!(config.connect_grace_ms.is_none());
        assert!(config.clients.is_none());
        assert!(config.authorized_fingerprints.is_none());
    }

    #[test]
    fn new_tuning_options_parse() {
        let config = parse("motion_batch_ms = 8\nconnect_grace_ms = 250\n");
        assert_eq!(config.motion_batch_ms, Some(8));
        assert_eq!(config.connect_grace_ms, Some(250));
    }

    #[test]
    fn unknown_config_keys_are_tolerated() {
        let config = parse("some_future_option = true\nmotion_batch_ms = 2\n");
        assert_eq!(config.motion_batch_ms, Some(2));
    }

    #[test]
    fn client_defaults_are_applied() {
        let client: ConfigClient = toml_client().into();
        assert_eq!(client.port, DEFAULT_PORT);
        assert_eq!(client.pos, Position::Left);
        assert!(!client.active);
        assert_eq!(client.ips.len(), 1);
        assert_eq!(client.hostname.as_deref(), Some("peer"));
    }

    #[test]
    fn activate_on_startup_maps_to_active() {
        let mut toml = toml_client();
        toml.activate_on_startup = Some(true);
        toml.port = Some(5000);
        toml.position = Some(Position::Right);
        let client: ConfigClient = toml.into();
        assert!(client.active);
        assert_eq!(client.port, 5000);
        assert_eq!(client.pos, Position::Right);
    }

    #[test]
    fn default_config_roundtrips_through_toml() {
        let serialized = toml::to_string_pretty(&ConfigToml::default()).expect("serialize");
        let parsed: ConfigToml = toml::from_str(&serialized).expect("reparse");
        assert!(parsed.port.is_none());
        assert!(parsed.clients.is_none());
    }

    #[test]
    fn default_network_constants_are_documented_values() {
        assert_eq!(DEFAULT_MOTION_BATCH_MS, 2);
        assert_eq!(DEFAULT_CONNECT_GRACE_MS, 400);
        assert_eq!(DEFAULT_RELEASE_KEYS.len(), 4);
    }

    #[test]
    fn clipboard_config_parses_and_applies_custom_limits() {
        let config = parse(
            "[clipboard]\nenabled = false\nmax_text_bytes = 10\nmax_html_bytes = 20\n             max_image_bytes = 30\nmax_snapshot_bytes = 40\n",
        );
        let clipboard = config.clipboard.as_ref().expect("clipboard table");
        assert!(!clipboard.enabled());
        let limits = clipboard.limits();
        assert_eq!(limits.max_text, 10);
        assert_eq!(limits.max_html, 20);
        assert_eq!(limits.max_image, 30);
        assert_eq!(limits.max_snapshot, 40);
    }

    #[test]
    fn clipboard_is_enabled_by_default() {
        assert!(ClipboardConfig::default().enabled());
        let config = parse("[clipboard]\nmax_text_bytes = 10\n");
        let clipboard = config.clipboard.as_ref().expect("clipboard table");
        assert!(clipboard.enabled());
        assert_eq!(clipboard.limits().max_text, 10);
        assert_eq!(
            clipboard.limits().max_image,
            input_clipboard::Limits::default().max_image,
            "unset fields fall back to the defaults"
        );
    }

    #[test]
    fn local_commit_is_eight_bytes() {
        let commit = local_commit();
        assert_eq!(commit.len(), 8);
    }
}
