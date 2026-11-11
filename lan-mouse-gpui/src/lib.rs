use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};

mod cursor_extension;
mod i18n;

use gpui::{
    App, Application, Bounds, ClipboardItem, Context, Entity, FocusHandle, KeyDownEvent,
    MouseButton, Render, SharedString, TitlebarOptions, Window, WindowBounds, WindowDecorations,
    WindowOptions, div, prelude::*, rgb, size,
};
use i18n::Language;
use lan_mouse_ipc::{
    AddressRuleView, ClientConfig, ClientHandle, ClientState, ClipboardStatus, DeviceView,
    Diagnostics, DirectionPolicy, DiscoveredDevice, FrontendEvent, FrontendRequest,
    FrontendRequestWriter, HistoryEntry, PendingView, Position, Status,
};

mod ui_state;
use ui_state::{
    LINK_FRESH_WINDOW_MS, LinkState, RowAction, link_state, outgoing_sensitivity_visible,
    row_action,
};

/// UI zoom applied to every layout/font size.
///
/// GPUI scales logical pixels by the display scale factor, but a 4K monitor
/// running at 100% still has far more logical pixels than a 1080p one, so the
/// same 980×720 window looks tiny. Scale the logical UI proportionally to the
/// display height (clamped), while letting an explicit `LAN_MOUSE_UI_SCALE`
/// override win for users who prefer a different size.
static UI_SCALE_PERMILLE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1000);

fn ui_scale() -> f32 {
    UI_SCALE_PERMILLE.load(std::sync::atomic::Ordering::Relaxed) as f32 / 1000.0
}

fn set_ui_scale(scale: f32) {
    let permille = (scale.clamp(0.75, 2.5) * 1000.0).round() as u32;
    UI_SCALE_PERMILLE.store(permille, std::sync::atomic::Ordering::Relaxed);
}

/// Set when the user explicitly asked to quit (tray/settings "Quit Lan Mouse").
///
/// The GPUI platform loop returns both when the app is asked to quit and when
/// the last window is destroyed, so this flag is the only way to tell the two
/// apart after `Application::run` returns: a plain window close must leave the
/// service (and the LAN connection) running, an explicit quit must stop it.
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

fn request_quit() {
    QUIT_REQUESTED.store(true, Ordering::SeqCst);
}

/// Drop-in replacement for `gpui::px` so every existing layout constant and
/// font size participates in the UI zoom.
fn px(value: f32) -> gpui::Pixels {
    gpui::px(value * ui_scale())
}

fn automatic_ui_scale(cx: &App) -> f32 {
    if let Ok(value) = std::env::var("LAN_MOUSE_UI_SCALE")
        && let Ok(scale) = value.parse::<f32>()
        && (0.75..=2.5).contains(&scale)
    {
        return scale;
    }
    let logical_height = cx
        .primary_display()
        .map(|display| f32::from(display.bounds().size.height))
        .unwrap_or(1080.0);
    (logical_height / 1080.0).clamp(1.0, 1.75)
}

const BG: u32 = 0x0f1115;
const SIDEBAR: u32 = 0x14171d;
const CARD: u32 = 0x171b22;
const BORDER: u32 = 0x262c36;
const TEXT: u32 = 0xe8edf4;
const MUTED: u32 = 0x98a2b3;
const ACCENT: u32 = 0x5aa9ff;
const GREEN: u32 = 0x4ade80;
const AMBER: u32 = 0xfbbf24;
const RED: u32 = 0xf87171;

#[cfg(all(feature = "tray", target_os = "linux"))]
mod tray {
    use async_channel::Sender;

    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Command {
        Open,
        Quit,
    }

    struct LanMouseTray {
        tx: Sender<Command>,
    }

    impl ksni::Tray for LanMouseTray {
        fn id(&self) -> String {
            "de.feschber.LanMouse".into()
        }

        fn title(&self) -> String {
            "Lan Mouse".into()
        }

        fn icon_name(&self) -> String {
            "de.feschber.LanMouse".into()
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::StandardItem;
            vec![
                StandardItem {
                    label: "Open Lan Mouse".into(),
                    icon_name: "window-new".into(),
                    activate: Box::new(|this: &mut Self| {
                        let _ = this.tx.send_blocking(Command::Open);
                    }),
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: "Quit Lan Mouse".into(),
                    icon_name: "application-exit".into(),
                    activate: Box::new(|this: &mut Self| {
                        let _ = this.tx.send_blocking(Command::Quit);
                    }),
                    ..Default::default()
                }
                .into(),
            ]
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            let _ = self.tx.send_blocking(Command::Open);
        }
    }

    pub(crate) fn spawn() -> async_channel::Receiver<Command> {
        let (tx, rx) = async_channel::unbounded();
        // A failed thread spawn (resource exhaustion) must not panic the UI at
        // startup: without a tray the window simply has no Open/Quit menu, which
        // is handled by the in-window Quit button.
        let spawn_result = std::thread::Builder::new()
            .name("lan-mouse-tray".into())
            .spawn(move || {
                use ksni::TrayMethods;
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        log::warn!("could not create tray runtime: {error}");
                        return;
                    }
                };
                runtime.block_on(async move {
                    let tray = LanMouseTray { tx };
                    match tray.spawn().await {
                        Ok(_handle) => std::future::pending::<()>().await,
                        Err(error) => log::warn!("system tray unavailable: {error}"),
                    }
                });
            });
        if let Err(error) = spawn_result {
            log::warn!("could not spawn the system tray thread: {error}");
        }
        rx
    }
}

#[cfg(all(feature = "tray", windows))]
mod tray {
    use async_channel::Sender;
    use tray_icon::{
        Icon, TrayIcon, TrayIconBuilder,
        menu::{Menu, MenuEvent, MenuItem},
    };

    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Command {
        Open,
        Quit,
    }

    fn icon() -> Icon {
        // The build script embeds assets/icon.ico into this executable, so the
        // notification-area icon matches the window/taskbar icon instead of a
        // solid placeholder. Fall back only if the resource is unavailable.
        if let Ok(icon) = Icon::from_resource(1, Some((32, 32))) {
            return icon;
        }
        let mut rgba = vec![0u8; 16 * 16 * 4];
        for pixel in rgba.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0x5a, 0xa9, 0xff, 0xff]);
        }
        Icon::from_rgba(rgba, 16, 16).expect("valid tray icon")
    }

    pub(crate) fn spawn() -> async_channel::Receiver<Command> {
        let (tx, rx) = async_channel::unbounded();
        let click_tx: Sender<Command> = tx.clone();
        TrayIconEventHandler::install(click_tx);

        let open = MenuItem::new("Open Lan Mouse", true, None);
        let quit = MenuItem::new("Quit Lan Mouse", true, None);
        let open_id = open.id().clone();
        let quit_id = quit.id().clone();
        let menu = Menu::new();
        let _ = menu.append(&open);
        let _ = menu.append(&quit);

        let menu_tx: Sender<Command> = tx;
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id() == &open_id {
                let _ = menu_tx.send_blocking(Command::Open);
            } else if event.id() == &quit_id {
                let _ = menu_tx.send_blocking(Command::Quit);
            }
        }));

        match TrayIconBuilder::new()
            .with_tooltip("Lan Mouse")
            .with_icon(icon())
            .with_menu(Box::new(menu))
            .build()
        {
            Ok(tray) => {
                // The Windows tray handle lives for the whole process; leak it
                // rather than storing it in a static (the platform handle is
                // !Send and must stay on the UI thread).
                let _: &'static mut TrayIcon = Box::leak(Box::new(tray));
            }
            Err(error) => log::warn!("Windows system tray unavailable: {error}"),
        }
        rx
    }

    struct TrayIconEventHandler;

    impl TrayIconEventHandler {
        fn install(tx: Sender<Command>) {
            tray_icon::TrayIconEvent::set_event_handler(Some(move |_event| {
                let _ = tx.send_blocking(Command::Open);
            }));
        }
    }
}

/// Fallback for targets without a native tray implementation.
///
/// Linux uses StatusNotifierItem and Windows the `tray-icon` crate; other
/// targets (macOS is documented as unsupported/dormant) must still compile, so
/// they get an inert channel instead of an unresolved module.
#[cfg(not(all(feature = "tray", any(target_os = "linux", windows))))]
mod tray {
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Command {
        Open,
        Quit,
    }

    pub(crate) fn spawn() -> async_channel::Receiver<Command> {
        let (_tx, rx) = async_channel::unbounded();
        rx
    }
}

/// Milliseconds since the Unix epoch, matching the clock the service uses for
/// [`ClientState::last_pong_at`].
fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

/// Char-safe fingerprint truncation for badges and labels.
///
/// The service normalizes fingerprints to hex today, but a config file can carry
/// arbitrary text: slicing a `String` by byte offset panics on a multi-byte
/// boundary, so the UI must never do that with untrusted text.
fn short_fingerprint(fingerprint: &str, max_chars: usize) -> String {
    let mut short: String = fingerprint.chars().take(max_chars).collect();
    if fingerprint.chars().count() > max_chars {
        short.push('…');
    }
    short
}

/// Normalized certificate fingerprint of a configured client, if it carries one.
///
/// Only the fingerprint is peer identity: two configured clients with the same
/// fingerprint are the same device (a service entry per address family), while a
/// hostname or IP is not proof of identity.
fn normalized_fingerprint(config: &ClientConfig) -> Option<String> {
    config
        .fingerprint
        .as_deref()
        .map(str::trim)
        .filter(|fingerprint| !fingerprint.is_empty())
        .map(str::to_ascii_lowercase)
}

/// One logical peer rendered as a single card: every configured client whose
/// certificate fingerprint is the same device.
#[derive(Clone, Debug, PartialEq)]
struct PeerAggregate {
    primary: ClientHandle,
    members: Vec<ClientHandle>,
    name: String,
    endpoint: String,
    position: Position,
    sensitivity: u16,
    active: bool,
    alive: bool,
    has_outgoing: bool,
    has_incoming: bool,
    last_pong_at: Option<u64>,
    rtt_ms: Option<u32>,
    jitter_ms: Option<u32>,
    loss_percent: Option<u8>,
    peer_commit: Option<[u8; 8]>,
    last_error: Option<String>,
}

/// Merge the configured clients that belong to one logical peer.
///
/// `active` is the OR of the members; the link fields come from a member with an
/// established outgoing link (preferring one that already produced a round-trip)
/// so duplicate entries cannot make the card flap between address families.
fn aggregate_peer(members: &[(ClientHandle, ClientConfig, ClientState)]) -> Option<PeerAggregate> {
    let (primary, first_config, _) = members.first()?;
    let link_index = members
        .iter()
        .position(|(_, _, state)| state.active_addr.is_some() && state.last_pong_at.is_some())
        .or_else(|| {
            members
                .iter()
                .position(|(_, _, state)| state.active_addr.is_some())
        })
        .unwrap_or(0);
    let (_, link_config, link_state) = &members[link_index];
    let hostname = members.iter().find_map(|(_, config, _)| {
        config
            .hostname
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    });
    let endpoint = link_state
        .active_addr
        .or_else(|| members.iter().find_map(|(_, _, state)| state.incoming_addr))
        .map(|addr| addr.to_string())
        .or_else(|| {
            members
                .iter()
                .find_map(|(_, config, _)| config.fix_ips.first().map(ToString::to_string))
        })
        .unwrap_or_else(|| format!("port {}", first_config.port));
    let name = hostname.clone().unwrap_or_else(|| {
        members
            .iter()
            .find_map(|(_, config, _)| config.fix_ips.first().map(ToString::to_string))
            .unwrap_or_else(|| "Unknown device".to_string())
    });
    let has_outgoing = members
        .iter()
        .any(|(_, _, state)| state.active_addr.is_some());
    let has_incoming = members
        .iter()
        .any(|(_, _, state)| state.incoming_addr.is_some());
    Some(PeerAggregate {
        primary: *primary,
        members: members.iter().map(|(handle, _, _)| *handle).collect(),
        name,
        endpoint,
        position: link_config.pos,
        sensitivity: first_config.sensitivity,
        active: members.iter().any(|(_, _, state)| state.active),
        alive: link_state.alive,
        has_outgoing,
        has_incoming,
        last_pong_at: members
            .iter()
            .filter_map(|(_, _, state)| state.last_pong_at)
            .max(),
        rtt_ms: link_state
            .rtt_ms
            .or_else(|| members.iter().find_map(|(_, _, state)| state.rtt_ms)),
        jitter_ms: link_state.jitter_ms,
        loss_percent: link_state.loss_percent,
        peer_commit: members.iter().find_map(|(_, _, state)| state.peer_commit),
        last_error: link_state.last_error.clone().or_else(|| {
            members
                .iter()
                .find_map(|(_, _, state)| state.last_error.clone())
        }),
    })
}

/// A new endpoint must persist this long before replacing the one shown on a
/// card. Prevents IPv4/IPv6 alternation when the service reports a different
/// source address per packet (same debounce as the GTK frontend).
const ENDPOINT_STICKY_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);

/// Debounced endpoint memory for one logical peer.
#[derive(Clone, Debug, Default)]
struct EndpointUi {
    shown: Option<String>,
    candidate: Option<(String, std::time::Instant)>,
}

/// Debounce a flapping endpoint so the card does not alternate between address
/// families on every packet.
///
/// `shown`/`candidate` are caller-owned memory (one pair per peer). A new
/// endpoint replaces the shown one immediately when nothing was shown yet, and
/// only after it persisted for `hold` otherwise. Returns the endpoint that
/// should be displayed now.
fn sticky_endpoint(
    shown: &mut Option<String>,
    candidate: &mut Option<(String, std::time::Instant)>,
    raw: Option<String>,
    now: std::time::Instant,
    hold: std::time::Duration,
) -> Option<String> {
    let Some(raw) = raw else {
        *candidate = None;
        *shown = None;
        return None;
    };
    if shown.as_deref() == Some(raw.as_str()) {
        *candidate = None;
        return Some(raw);
    }
    let Some(previous) = shown.clone() else {
        *shown = Some(raw.clone());
        *candidate = None;
        return Some(raw);
    };
    match candidate {
        Some((pending, since)) if pending == &raw => {
            if now.saturating_duration_since(*since) >= hold {
                *shown = Some(raw.clone());
                *candidate = None;
                Some(raw)
            } else {
                Some(previous)
            }
        }
        _ => {
            *candidate = Some((raw, now));
            Some(previous)
        }
    }
}

/// Group client handles into logical peers by certificate fingerprint.
///
/// Handles are sorted first so the card order is deterministic. Two configured
/// clients with the same fingerprint are the same device (the service can hold
/// one entry per address family); entries without a fingerprint stay separate on
/// purpose, because hiding a real device would be worse than a duplicate.
fn group_peer_handles(
    clients: &HashMap<ClientHandle, (ClientConfig, ClientState)>,
) -> Vec<Vec<ClientHandle>> {
    let mut handles = clients.keys().copied().collect::<Vec<_>>();
    handles.sort_unstable();
    let mut groups: Vec<(Option<String>, Vec<ClientHandle>)> = Vec::new();
    for handle in handles {
        let identity = clients
            .get(&handle)
            .and_then(|(config, _)| normalized_fingerprint(config));
        match identity {
            Some(identity) => match groups
                .iter_mut()
                .find(|(key, _)| key.as_deref() == Some(identity.as_str()))
            {
                Some((_, members)) => members.push(handle),
                None => groups.push((Some(identity), vec![handle])),
            },
            None => groups.push((None, vec![handle])),
        }
    }
    groups.into_iter().map(|(_, members)| members).collect()
}

/// Human-friendly age of a Unix timestamp in seconds (audit history rows).
fn format_timestamp_age(ts_secs: u64, now_ms: u64) -> String {
    let age = now_ms / 1000;
    let age = age.saturating_sub(ts_secs);
    match age {
        0..=59 => format!("{age}s ago"),
        60..=3599 => format!("{}m ago", age / 60),
        3600..=86_399 => format!("{}h ago", age / 3600),
        _ => format!("{}d ago", age / 86_400),
    }
}

/// Whether a client pinned to `fingerprint` has a round-trip proof younger than
/// the freshness window.
fn has_fresh_pong(
    clients: &HashMap<ClientHandle, (ClientConfig, ClientState)>,
    fingerprint: &str,
    now_ms: u64,
) -> bool {
    let identity = fingerprint.trim();
    if identity.is_empty() {
        return false;
    }
    clients.values().any(|(config, state)| {
        normalized_fingerprint(config).is_some_and(|known| known.eq_ignore_ascii_case(identity))
            && state
                .last_pong_at
                .is_some_and(|seen| now_ms.saturating_sub(seen) <= LINK_FRESH_WINDOW_MS)
    })
}

/// Sides a peer can be placed on, in the order shown in the UI.
const SIDES: [Position; 4] = [
    Position::Left,
    Position::Right,
    Position::Top,
    Position::Bottom,
];

fn position_label(position: Position) -> &'static str {
    match position {
        Position::Left => "left",
        Position::Right => "right",
        Position::Top => "top",
        Position::Bottom => "bottom",
    }
}

/// Best-effort IPv4 addresses for the "This Computer" card.
fn discover_local_ipv4() -> Vec<String> {
    // Prefer the address of the default route: that is the interface a peer
    // on the LAN can reach. A UDP connect selects the route without sending a
    // packet.
    if let Some(ip) = default_route_ipv4() {
        return vec![ip.to_string()];
    }

    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut candidates = interfaces
        .into_iter()
        .filter_map(|interface| match interface.addr {
            if_addrs::IfAddr::V4(v4) if !v4.ip.is_loopback() && !v4.ip.is_link_local() => {
                let name = interface.name.to_ascii_lowercase();
                let virtual_interface = [
                    "docker", "br-", "veth", "virbr", "podman", "cni", "flannel", "tun", "tap",
                ]
                .iter()
                .any(|prefix| name.starts_with(prefix));
                (!virtual_interface).then_some(v4.ip)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.dedup();
    candidates
        .into_iter()
        .next()
        .map(|ip| vec![ip.to_string()])
        .unwrap_or_default()
}

fn default_route_ipv4() -> Option<std::net::Ipv4Addr> {
    let socket = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(("8.8.8.8", 80)).ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

#[derive(Clone, Debug)]
enum PendingAdd {
    Discovered {
        ip: std::net::IpAddr,
        port: u16,
        position: Position,
        fingerprint: Option<String>,
    },
    Manual {
        hostname: String,
        port: u16,
        fingerprint: Option<String>,
        position: Position,
    },
}

/// Whether a `Created` event is the empty client produced by `Create`.
///
/// `Created` is also emitted for auto-reverse clients and for every entry on a
/// config reload. Applying a pending Add to one of those was the Network-page
/// bug where "Add" configured the wrong card (or appeared to pick another IP)
/// while the real new client stayed empty.
fn is_fresh_client_config(config: &ClientConfig) -> bool {
    config
        .hostname
        .as_deref()
        .is_none_or(|name| name.trim().is_empty())
        && config.fix_ips.is_empty()
        && config.fingerprint.is_none()
        && config.port == lan_mouse_ipc::DEFAULT_PORT
}

/// Consume the pending Add only when `Created` belongs to the empty client we
/// just asked the service to create.
fn take_pending_add(pending: &mut Option<PendingAdd>, config: &ClientConfig) -> Option<PendingAdd> {
    if is_fresh_client_config(config) {
        pending.take()
    } else {
        None
    }
}

#[derive(Clone, Debug)]
struct AddDeviceDialog {
    hostname: String,
    port: String,
    fingerprint: String,
    position: Position,
    field: AddField,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AddField {
    Hostname,
    Port,
    Fingerprint,
}

#[derive(Clone, Debug)]
enum SettingsDialogKind {
    Address,
    Certificate,
    /// All configured clients of one logical peer (a peer can have one entry per
    /// address family); an edit is applied to every member so duplicates cannot
    /// diverge.
    Client {
        members: Vec<ClientHandle>,
    },
    RenameDevice {
        fingerprint: String,
    },
    Port,
}

#[derive(Clone, Debug)]
struct SettingsDialog {
    kind: SettingsDialogKind,
    hostname: String,
    port: String,
    address: String,
    name: String,
    fingerprint: String,
    field: u8,
}

impl SettingsDialog {
    fn new_address() -> Self {
        Self {
            kind: SettingsDialogKind::Address,
            hostname: String::new(),
            port: String::new(),
            address: String::new(),
            name: String::new(),
            fingerprint: String::new(),
            field: 0,
        }
    }

    fn new_certificate() -> Self {
        Self {
            kind: SettingsDialogKind::Certificate,
            hostname: String::new(),
            port: String::new(),
            address: String::new(),
            name: String::new(),
            fingerprint: String::new(),
            field: 0,
        }
    }

    fn new_client(members: Vec<ClientHandle>, config: &ClientConfig) -> Self {
        Self {
            kind: SettingsDialogKind::Client { members },
            hostname: config.hostname.clone().unwrap_or_default(),
            port: config.port.to_string(),
            address: String::new(),
            name: String::new(),
            fingerprint: String::new(),
            field: 0,
        }
    }

    fn new_rename(fingerprint: String, name: String) -> Self {
        Self {
            kind: SettingsDialogKind::RenameDevice { fingerprint },
            hostname: String::new(),
            port: String::new(),
            address: String::new(),
            name,
            fingerprint: String::new(),
            field: 0,
        }
    }

    fn new_port(current: u16) -> Self {
        Self {
            kind: SettingsDialogKind::Port,
            hostname: String::new(),
            port: current.to_string(),
            address: String::new(),
            name: String::new(),
            fingerprint: String::new(),
            field: 0,
        }
    }

    fn field_count(&self) -> u8 {
        match self.kind {
            SettingsDialogKind::Address => 1,
            SettingsDialogKind::Certificate | SettingsDialogKind::Client { .. } => 2,
            SettingsDialogKind::RenameDevice { .. } | SettingsDialogKind::Port => 1,
        }
    }

    fn value_mut(&mut self) -> &mut String {
        match self.kind {
            SettingsDialogKind::Address => &mut self.address,
            SettingsDialogKind::Certificate => {
                if self.field == 0 {
                    &mut self.name
                } else {
                    &mut self.fingerprint
                }
            }
            SettingsDialogKind::Client { .. } => {
                if self.field == 0 {
                    &mut self.hostname
                } else {
                    &mut self.port
                }
            }
            SettingsDialogKind::RenameDevice { .. } => &mut self.name,
            SettingsDialogKind::Port => &mut self.port,
        }
    }
}

impl AddDeviceDialog {
    fn new() -> Self {
        Self {
            hostname: String::new(),
            port: lan_mouse_ipc::DEFAULT_PORT.to_string(),
            fingerprint: String::new(),
            position: Position::Left,
            field: AddField::Hostname,
        }
    }

    fn value_mut(&mut self) -> &mut String {
        match self.field {
            AddField::Hostname => &mut self.hostname,
            AddField::Port => &mut self.port,
            AddField::Fingerprint => &mut self.fingerprint,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct MetricSample {
    rtt_ms: u32,
    jitter_ms: u32,
    loss_percent: u8,
}

enum BridgeMessage {
    Writer(FrontendRequestWriter),
    Event(Box<FrontendEvent>),
    Error(String),
    /// The service closed the IPC stream (crash or shutdown).
    Closed,
}

struct Dashboard {
    writer: Option<FrontendRequestWriter>,
    language: Language,
    clients: HashMap<ClientHandle, (ClientConfig, ClientState)>,
    pending: Vec<PendingView>,
    devices: Vec<DeviceView>,
    discovered: Vec<DiscoveredDevice>,
    pending_add: Option<PendingAdd>,
    add_dialog: Option<AddDeviceDialog>,
    settings_dialog: Option<SettingsDialog>,
    add_focus: FocusHandle,
    address_rules: Vec<AddressRuleView>,
    authorized: std::collections::HashMap<String, String>,
    status: String,
    page: Page,
    selected_client: Option<ClientHandle>,
    hostname: String,
    local_ips: Vec<String>,
    public_fingerprint: Option<String>,
    port: u16,
    capture_status: Status,
    emulation_status: Status,
    /// sensitivity of input received from peers through the absolute pointer
    absolute_sensitivity: u16,
    clipboard_status: ClipboardStatus,
    pairing_active: bool,
    pairing_review: Vec<DeviceView>,
    local_commit: [u8; 8],
    diagnostics: Diagnostics,
    /// Optional GNOME Wayland extension used to hide the local cursor.
    cursor_extension: cursor_extension::CursorExtensionState,
    metric_history: HashMap<ClientHandle, VecDeque<MetricSample>>,
    history: Vec<HistoryEntry>,
    policy_export: Option<String>,
    /// Debounced endpoint shown per logical peer (first member handle).
    shown_endpoints: HashMap<ClientHandle, EndpointUi>,
    /// When the user last requested a network scan (rate limit).
    scan_requested_at: Option<std::time::Instant>,
    first_frame_logged: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Devices,
    Network,
    Settings,
}

/// Per-device element id for the Network "Add" button.
///
/// All rows used the same static `add-discovered` id. GPUI dispatches clicks
/// through element ids, so every row could resolve to the first row's hitbox
/// and Add configured another IP (or appeared to do nothing).
fn network_add_id(ip: std::net::IpAddr, port: u16) -> SharedString {
    SharedString::from(format!("add-discovered-{ip}-{port}"))
}

impl Dashboard {
    fn new(cx: &mut Context<Self>, local_commit: [u8; 8]) -> Self {
        let (tx, rx) = async_channel::unbounded();

        std::thread::spawn(move || {
            let (mut reader, writer) = match lan_mouse_ipc::connect() {
                Ok(connection) => connection,
                Err(error) => {
                    let _ = tx.send_blocking(BridgeMessage::Error(error.to_string()));
                    return;
                }
            };
            if tx.send_blocking(BridgeMessage::Writer(writer)).is_err() {
                return;
            }
            loop {
                match reader.next_event() {
                    Some(Ok(event)) => {
                        if tx
                            .send_blocking(BridgeMessage::Event(Box::new(event)))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Some(Err(error)) => log::warn!("frontend event error: {error}"),
                    None => {
                        // The service closed the IPC socket (crash or exit).
                        // Tell the UI instead of silently freezing on stale
                        // state: every request from then on is lost.
                        let _ = tx.send_blocking(BridgeMessage::Closed);
                        return;
                    }
                }
            }
        });

        cx.spawn(async move |this, cx| {
            while let Ok(message) = rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    this.handle_bridge(message);
                    cx.notify();
                });
            }
        })
        .detach();

        // Round-trip freshness decays with time, so the view must be refreshed
        // even when the service sends no further events; otherwise a silent peer
        // would keep showing "Connected" forever (the GTK frontend has the same
        // 1 s status tick). State itself still comes only from the service.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();

        // On GNOME Wayland the cursor can only be hidden by the bundled Shell
        // extension. Install/enable it on first run and tell the user that one
        // logout/login is required for the Shell to load it.
        let mut cursor_extension = cursor_extension::detect();
        if cursor_extension.needed && (!cursor_extension.enabled || cursor_extension.hint.is_some())
        {
            match cursor_extension::install() {
                Ok(state) => cursor_extension = state,
                Err(error) => {
                    log::warn!("could not install the GNOME cursor extension: {error}");
                }
            }
        }

        Self {
            writer: None,
            language: Language::load(),
            clients: HashMap::new(),
            pending: Vec::new(),
            devices: Vec::new(),
            discovered: Vec::new(),
            pending_add: None,
            add_dialog: None,
            settings_dialog: None,
            add_focus: cx.focus_handle(),
            address_rules: Vec::new(),
            authorized: std::collections::HashMap::new(),
            status: Language::load()
                .tr("Connecting to Lan Mouse service…")
                .to_string(),
            page: Page::Devices,
            selected_client: None,
            hostname: hostname::get()
                .ok()
                .and_then(|name| name.into_string().ok())
                .unwrap_or_else(|| "This computer".to_string()),
            local_ips: discover_local_ipv4(),
            public_fingerprint: None,
            port: lan_mouse_ipc::DEFAULT_PORT,
            capture_status: Status::Disabled,
            emulation_status: Status::Disabled,
            absolute_sensitivity: 100,
            clipboard_status: ClipboardStatus::default(),
            pairing_active: false,
            pairing_review: Vec::new(),
            local_commit,
            diagnostics: Diagnostics::default(),
            cursor_extension,
            metric_history: HashMap::new(),
            history: Vec::new(),
            policy_export: None,
            shown_endpoints: HashMap::new(),
            scan_requested_at: None,
            first_frame_logged: false,
        }
    }

    fn tr(&self, english: &'static str) -> &'static str {
        self.language.tr(english)
    }

    fn set_language(&mut self, language: Language, cx: &mut Context<Self>) {
        if self.language == language {
            return;
        }
        self.language = language;
        language.save();
        self.status = match language {
            Language::English => "Language: English".to_string(),
            Language::PortugueseBrazil => "Idioma: Português (Brasil)".to_string(),
        };
        cx.notify();
    }

    fn handle_bridge(&mut self, message: BridgeMessage) {
        match message {
            BridgeMessage::Writer(mut writer) => {
                let _ = writer.request(FrontendRequest::Hello {
                    ipc_version: lan_mouse_ipc::IPC_VERSION,
                });
                let _ = writer.request(FrontendRequest::Enumerate());
                let _ = writer.request(FrontendRequest::ListDevices);
                let _ = writer.request(FrontendRequest::ListPending);
                let _ = writer.request(FrontendRequest::ListAddressRules);
                let _ = writer.request(FrontendRequest::ListHistory { limit: 20 });
                let _ = writer.request(FrontendRequest::Sync);
                self.writer = Some(writer);
                self.status = self.tr("Connected to service").to_string();
                log::info!("GPUI frontend connected to the Lan Mouse service");
            }
            BridgeMessage::Event(event) => self.handle_event(*event),
            BridgeMessage::Error(error) => {
                self.status = format!("Service error: {error}");
            }
            BridgeMessage::Closed => {
                // Keep showing the last state but never pretend it is live: the
                // service is gone, so every subsequent request is lost.
                self.writer = None;
                self.status =
                    "Lost connection to the Lan Mouse service — restart Lan Mouse".to_string();
            }
        }
    }

    fn handle_event(&mut self, event: FrontendEvent) {
        match event {
            FrontendEvent::Enumerate(rows) => {
                self.clients.clear();
                // Endpoint debounce state belongs to the snapshot being replaced.
                self.shown_endpoints.clear();
                for (handle, config, state) in rows {
                    self.remember_metrics(handle, &state);
                    self.clients.insert(handle, (config, state));
                }
                log::info!("GPUI frontend received {} client(s)", self.clients.len());
            }
            FrontendEvent::Created(handle, config, state) => {
                self.remember_metrics(handle, &state);
                // Decide against the config before it is moved into the map.
                let pending_for_created = take_pending_add(&mut self.pending_add, &config);
                self.clients.insert(handle, (config, state));
                if let Some(pending) = pending_for_created {
                    match pending {
                        PendingAdd::Discovered {
                            ip,
                            port,
                            position,
                            fingerprint,
                        } => {
                            self.send(FrontendRequest::UpdateFixIps(handle, vec![ip]));
                            self.send(FrontendRequest::UpdatePort(handle, port));
                            self.send(FrontendRequest::UpdatePosition(handle, position));
                            if let Some(fingerprint) = fingerprint {
                                self.send(FrontendRequest::UpdateFingerprint(
                                    handle,
                                    Some(fingerprint),
                                ));
                            }
                        }
                        PendingAdd::Manual {
                            hostname,
                            port,
                            fingerprint,
                            position,
                        } => {
                            let hostname = hostname.trim();
                            if !hostname.is_empty() {
                                self.send(FrontendRequest::UpdateHostname(
                                    handle,
                                    Some(hostname.to_string()),
                                ));
                            }
                            self.send(FrontendRequest::UpdatePort(handle, port));
                            if let Some(fingerprint) = fingerprint {
                                self.send(FrontendRequest::UpdateFingerprint(
                                    handle,
                                    Some(fingerprint),
                                ));
                            }
                            self.send(FrontendRequest::UpdatePosition(handle, position));
                        }
                    }
                    self.send(FrontendRequest::Activate(handle, true));
                }
            }
            FrontendEvent::State(handle, config, state) => {
                self.remember_metrics(handle, &state);
                self.clients.insert(handle, (config, state));
            }
            FrontendEvent::Deleted(handle) => {
                self.clients.remove(&handle);
                self.metric_history.remove(&handle);
                self.shown_endpoints.remove(&handle);
                if self.selected_client == Some(handle) {
                    self.selected_client = None;
                }
            }
            FrontendEvent::PendingRequests(pending) => self.pending = pending,
            FrontendEvent::Hello { ipc_version } => {
                self.status = format!("Service connected (IPC v{ipc_version})");
            }
            FrontendEvent::Error(error) => {
                self.status = error;
                // A failed create must not leave a half-configured pending add
                // around to be applied to an unrelated client later.
                self.pending_add = None;
            }
            FrontendEvent::Devices(devices) => self.devices = devices,
            FrontendEvent::NetworkScan(devices) => self.discovered = devices,
            FrontendEvent::AddressRules(rules) => self.address_rules = rules,
            FrontendEvent::AuthorizedUpdated(authorized) => self.authorized = authorized,
            FrontendEvent::PublicKeyFingerprint(fingerprint) => {
                self.public_fingerprint = Some(fingerprint)
            }
            FrontendEvent::PortChanged(port, message) => {
                self.port = port;
                if let Some(message) = message {
                    self.status = message;
                }
            }
            FrontendEvent::AbsoluteSensitivity(percent) => {
                self.absolute_sensitivity = percent;
            }
            FrontendEvent::CaptureStatus(status) => self.capture_status = status,
            FrontendEvent::EmulationStatus(status) => self.emulation_status = status,
            FrontendEvent::ClipboardStatus(status) => self.clipboard_status = status,
            FrontendEvent::PairingActive(active) => self.pairing_active = active,
            FrontendEvent::PairingReview(devices) => self.pairing_review = devices,
            FrontendEvent::Diagnostics(diagnostics) => self.diagnostics = diagnostics,
            FrontendEvent::ConnectionHistory(history) => self.history = history,
            FrontendEvent::PolicyExport { json } => self.policy_export = Some(json),
            // The full request is shown as a card on the Devices page; a status
            // hint makes it visible without an extra IPC round-trip.
            FrontendEvent::ConnectionAttempt { addr, .. } => {
                self.status =
                    format!("Connection request from {addr} — review it on the Devices page");
            }
            FrontendEvent::CertificateChanged { name, .. } => {
                self.status = format!(
                    "Certificate changed for {name} — review the pending request before approving"
                );
            }
            _ => {}
        }
    }

    fn send(&mut self, request: FrontendRequest) {
        let Some(writer) = self.writer.as_mut() else {
            self.status = "Not connected to the Lan Mouse service yet".to_string();
            return;
        };
        if let Err(error) = writer.request(request) {
            self.status = format!("IPC write error: {error}");
        }
    }

    /// Apply the card's displayed action to every configured client of the peer.
    ///
    /// The activation bit is derived from the *displayed* action, never from the
    /// raw `state.active` flag: that is what kept the old "Connect" button
    /// disconnecting an already enabled client.
    fn apply_row_action(&mut self, members: &[ClientHandle], action: RowAction) {
        let activate = action.wants_activation();
        for handle in members {
            self.send(FrontendRequest::Activate(*handle, activate));
        }
        self.selected_client = members.first().copied();
    }

    /// Configured clients grouped into logical peers by certificate fingerprint.
    ///
    /// Two configured clients with the same fingerprint are the same device (the
    /// service can hold one entry per address family). Entries without a
    /// fingerprint stay separate on purpose: hiding a real device would be worse
    /// than showing a duplicate.
    fn peer_groups(&self) -> Vec<Vec<ClientHandle>> {
        group_peer_handles(&self.clients)
    }

    fn aggregate_members(&self, members: &[ClientHandle]) -> Option<PeerAggregate> {
        let entries = members
            .iter()
            .filter_map(|handle| {
                self.clients
                    .get(handle)
                    .map(|(config, state)| (*handle, config.clone(), state.clone()))
            })
            .collect::<Vec<_>>();
        aggregate_peer(&entries)
    }

    /// Aggregate and derive the honest state for one logical peer.
    fn peer_aggregate(&self, members: &[ClientHandle]) -> Option<(PeerAggregate, LinkState)> {
        let aggregate = self.aggregate_members(members)?;
        let state = link_state(
            aggregate.active,
            aggregate.alive,
            aggregate.has_outgoing,
            aggregate.has_incoming,
            aggregate.last_pong_at,
            unix_ms_now(),
        );
        Some((aggregate, state))
    }

    /// Number of peers with a fresh outgoing round-trip (sidebar counter).
    fn healthy_link_count(&self) -> usize {
        self.peer_groups()
            .iter()
            .filter(|members| {
                self.peer_aggregate(members).is_some_and(|(_, state)| {
                    matches!(
                        state,
                        LinkState::Connected
                            | LinkState::ConnectedBidirectional
                            | LinkState::ConnectedDegraded
                    )
                })
            })
            .count()
    }

    fn switch_page(&mut self, page: Page) {
        self.page = page;
        // Discovery is only started explicitly from the Network page/header.
        // Automatically probing every subnet when the tab opens caused
        // unnecessary traffic (and looked like a frozen UI on large networks).
    }

    fn open_address_dialog(&mut self) {
        self.settings_dialog = Some(SettingsDialog::new_address());
    }

    fn open_certificate_dialog(&mut self) {
        self.settings_dialog = Some(SettingsDialog::new_certificate());
    }

    fn open_client_edit(&mut self, members: &[ClientHandle]) {
        let Some(handle) = members.first() else {
            return;
        };
        if let Some((config, _)) = self.clients.get(handle) {
            self.settings_dialog = Some(SettingsDialog::new_client(members.to_vec(), config));
        }
    }

    fn open_port_dialog(&mut self) {
        self.settings_dialog = Some(SettingsDialog::new_port(self.port));
    }

    fn open_device_rename(&mut self, fingerprint: String, name: String) {
        self.settings_dialog = Some(SettingsDialog::new_rename(fingerprint, name));
    }

    fn close_settings_dialog(&mut self) {
        self.settings_dialog = None;
    }

    fn submit_settings_dialog(&mut self) {
        let Some(dialog) = self.settings_dialog.take() else {
            return;
        };
        match dialog.kind {
            SettingsDialogKind::Address => {
                let address = dialog.address.trim();
                if address.is_empty() {
                    return;
                }
                self.send(FrontendRequest::AddAddressRule {
                    address: address.to_string(),
                    incoming: DirectionPolicy::allow_all(),
                    outgoing: DirectionPolicy::allow_all(),
                });
            }
            SettingsDialogKind::Certificate => {
                let fingerprint = dialog.fingerprint.trim();
                if fingerprint.is_empty() {
                    return;
                }
                let name = dialog.name.trim();
                self.send(FrontendRequest::AuthorizeKey(
                    if name.is_empty() {
                        "authorized device".to_string()
                    } else {
                        name.to_string()
                    },
                    fingerprint.to_string(),
                ));
            }
            SettingsDialogKind::Port => {
                if let Ok(port) = dialog.port.trim().parse::<u16>() {
                    self.send(FrontendRequest::ChangePort(port));
                }
            }
            SettingsDialogKind::RenameDevice { fingerprint } => {
                let name = dialog.name.trim();
                if !name.is_empty() {
                    self.send(FrontendRequest::RenameDevice {
                        fingerprint,
                        name: name.to_string(),
                    });
                }
            }
            SettingsDialogKind::Client { members } => {
                let hostname = dialog.hostname.trim().to_string();
                let hostname = (!hostname.is_empty()).then_some(hostname);
                let port = match dialog.port.trim().parse::<u16>() {
                    Ok(port) => Some(port),
                    Err(_) => {
                        self.status = "Port must be a number between 1 and 65535".to_string();
                        None
                    }
                };
                for handle in members {
                    self.send(FrontendRequest::UpdateHostname(handle, hostname.clone()));
                    if let Some(port) = port {
                        self.send(FrontendRequest::UpdatePort(handle, port));
                    }
                }
            }
        }
    }

    fn on_settings_key(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "escape" => self.close_settings_dialog(),
            "enter" => self.submit_settings_dialog(),
            "tab" => {
                if let Some(dialog) = self.settings_dialog.as_mut() {
                    dialog.field = (dialog.field + 1) % dialog.field_count();
                }
            }
            "backspace" => {
                if let Some(dialog) = self.settings_dialog.as_mut() {
                    dialog.value_mut().pop();
                }
            }
            key if key.len() == 1
                && !event.keystroke.modifiers.control
                && !event.keystroke.modifiers.platform =>
            {
                if let Some(dialog) = self.settings_dialog.as_mut() {
                    dialog.value_mut().push_str(key);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn add_device(&mut self) {
        self.add_dialog = Some(AddDeviceDialog::new());
    }

    fn close_add_dialog(&mut self) {
        self.add_dialog = None;
    }

    fn submit_add_dialog(&mut self) {
        if self.pending_add.is_some() {
            // A second `Create` would be answered with another `Created` event,
            // and the single pending-add slot cannot be matched to either one;
            // the two clients would end up with the wrong address/config.
            self.status = "Waiting for the service to answer the previous add request".to_string();
            return;
        }
        let Some(dialog) = self.add_dialog.take() else {
            return;
        };
        let port = match dialog.port.trim().parse::<u16>() {
            Ok(port) => port,
            Err(_) => {
                // Keep the dialog open so the user can fix the value instead of
                // silently adding the device on the default port.
                self.status = "Port must be a number between 1 and 65535".to_string();
                self.add_dialog = Some(dialog);
                return;
            }
        };
        let fingerprint = if dialog.fingerprint.trim().is_empty() {
            None
        } else {
            Some(dialog.fingerprint.trim().to_string())
        };
        self.pending_add = Some(PendingAdd::Manual {
            hostname: dialog.hostname,
            port,
            fingerprint,
            position: dialog.position,
        });
        self.send(FrontendRequest::Create);
    }

    fn on_add_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => {
                self.close_add_dialog();
            }
            "enter" => {
                self.submit_add_dialog();
            }
            "tab" => {
                if let Some(dialog) = self.add_dialog.as_mut() {
                    dialog.field = match dialog.field {
                        AddField::Hostname => AddField::Port,
                        AddField::Port => AddField::Fingerprint,
                        AddField::Fingerprint => AddField::Hostname,
                    };
                }
            }
            "backspace" => {
                if let Some(dialog) = self.add_dialog.as_mut() {
                    dialog.value_mut().pop();
                }
            }
            key if key.len() == 1
                && !event.keystroke.modifiers.control
                && !event.keystroke.modifiers.platform =>
            {
                if let Some(dialog) = self.add_dialog.as_mut() {
                    dialog.value_mut().push_str(key);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn add_discovered(&mut self, ip: std::net::IpAddr, port: u16, fingerprint: Option<String>) {
        if self.pending_add.is_some() {
            self.status = "Waiting for the service to answer the previous add request".to_string();
            return;
        }
        self.pending_add = Some(PendingAdd::Discovered {
            ip,
            port,
            position: Position::Left,
            fingerprint,
        });
        self.send(FrontendRequest::Create);
    }

    /// Place a peer on the given screen side.
    ///
    /// Works both for configured clients and for connected peers that only
    /// receive input: the service moves the live capture zone immediately, so
    /// no disconnect/reconnect is needed.
    fn set_position(&mut self, members: &[ClientHandle], position: Position) {
        for handle in members {
            self.send(FrontendRequest::UpdatePosition(*handle, position));
        }
    }

    fn adjust_sensitivity(&mut self, members: &[ClientHandle], current: u16, delta: i32) {
        let next = (i32::from(current) + delta).clamp(10, 500) as u16;
        for handle in members {
            self.send(FrontendRequest::UpdateSensitivity(*handle, next));
        }
    }

    /// Start a network scan, at most once every few seconds.
    ///
    /// The service spawns a fresh discovery task per \`ScanNetwork\` request, so
    /// repeated clicks would run concurrent scans and flood the network with
    /// duplicate beacons.
    fn start_scan(&mut self) {
        let debounce = std::time::Duration::from_secs(3);
        if self
            .scan_requested_at
            .is_some_and(|at| at.elapsed() < debounce)
        {
            return;
        }
        self.scan_requested_at = Some(std::time::Instant::now());
        self.status = self.tr("Scanning the local network…").to_string();
        self.send(FrontendRequest::ScanNetwork);
    }

    fn delete_peer(&mut self, members: &[ClientHandle]) {
        for handle in members {
            self.send(FrontendRequest::Delete(*handle));
        }
    }

    fn set_policy(
        &mut self,
        fingerprint: String,
        incoming: DirectionPolicy,
        outgoing: DirectionPolicy,
    ) {
        self.send(FrontendRequest::SetDevicePolicy {
            fingerprint,
            incoming,
            outgoing,
        });
    }

    fn revoke_device(&mut self, fingerprint: String) {
        self.send(FrontendRequest::RevokeDevice {
            fingerprint,
            disconnect: true,
        });
    }

    fn dismiss_pairing_review(&mut self, fingerprint: String) {
        self.pairing_review
            .retain(|device| device.fingerprint != fingerprint);
    }

    fn approve_pending(&mut self, fingerprint: String) {
        self.respond_pending_with(
            fingerprint,
            DirectionPolicy::allow_all(),
            DirectionPolicy::allow_all(),
        );
    }

    fn respond_pending_with(
        &mut self,
        fingerprint: String,
        incoming: DirectionPolicy,
        outgoing: DirectionPolicy,
    ) {
        self.send(FrontendRequest::RespondPending {
            fingerprint,
            accept: true,
            name: None,
            position: Some(Position::Left),
            incoming: Some(incoming),
            outgoing: Some(outgoing),
        });
    }

    fn deny_pending(&mut self, fingerprint: String) {
        self.send(FrontendRequest::RespondPending {
            fingerprint,
            accept: false,
            name: None,
            position: None,
            incoming: None,
            outgoing: None,
        });
    }

    fn enable_capture(&mut self) {
        self.send(FrontendRequest::EnableCapture);
    }

    fn enable_emulation(&mut self) {
        self.send(FrontendRequest::EnableEmulation);
    }

    fn toggle_clipboard(&mut self) {
        self.send(FrontendRequest::SetClipboardEnabled(
            !self.clipboard_status.enabled,
        ));
    }

    fn start_pairing(&mut self) {
        self.send(FrontendRequest::StartPairing(60));
    }

    fn remember_metrics(&mut self, handle: ClientHandle, state: &ClientState) {
        let (Some(rtt_ms), jitter_ms, loss_percent) = (
            state.rtt_ms,
            state.jitter_ms.unwrap_or(0),
            state.loss_percent.unwrap_or(0),
        ) else {
            return;
        };
        let history = self.metric_history.entry(handle).or_default();
        history.push_back(MetricSample {
            rtt_ms,
            jitter_ms,
            loss_percent,
        });
        while history.len() > 40 {
            history.pop_front();
        }
    }

    fn sidebar_item(
        &self,
        page: Page,
        label: &'static str,
        icon: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let selected = self.page == page;
        div()
            .id(SharedString::from(label))
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .rounded_md()
            .cursor_pointer()
            .bg(if selected {
                rgb(0x1d2430)
            } else {
                rgb(SIDEBAR)
            })
            .text_color(if selected { rgb(TEXT) } else { rgb(MUTED) })
            .child(div().text_base().child(icon))
            .child(div().text_sm().child(label))
            .on_click(cx.listener(move |this, _, _, _| this.switch_page(page)))
    }

    /// Render one logical peer card (one or more configured clients sharing a
    /// certificate fingerprint) from the aggregated, proof-based state.
    fn render_peer(
        &mut self,
        members: &[ClientHandle],
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some((peer, state)) = self.peer_aggregate(members) else {
            return div().into_any_element();
        };
        let action = row_action(state, peer.active);
        let version_mismatch = peer
            .peer_commit
            .is_some_and(|peer_commit| peer_commit != self.local_commit);
        let mut link_metrics = peer
            .rtt_ms
            .map(|rtt| format!(" · RTT {rtt} ms"))
            .unwrap_or_default();
        if let Some(jitter) = peer.jitter_ms {
            link_metrics.push_str(&format!(" · jitter {jitter} ms"));
        }
        if let Some(loss) = peer.loss_percent {
            link_metrics.push_str(&format!(" · loss {loss}%"));
        }
        let now = unix_ms_now();
        let last_pong = peer
            .last_pong_at
            .map(|seen| format!(" · last Pong {}s ago", now.saturating_sub(seen) / 1000))
            .unwrap_or_default();
        let stale_secs = peer
            .last_pong_at
            .map(|seen| now.saturating_sub(seen) / 1000)
            .unwrap_or_default();
        // Debounce the displayed endpoint: the service can rewrite `active_addr`
        // on every Pong, which would make a dual-stack card flap between IPv4 and
        // IPv6. A new endpoint only replaces the shown one after it persisted for
        // the sticky window.
        let display_endpoint = {
            let ui = self.shown_endpoints.entry(peer.primary).or_default();
            sticky_endpoint(
                &mut ui.shown,
                &mut ui.candidate,
                Some(peer.endpoint.clone()),
                std::time::Instant::now(),
                ENDPOINT_STICKY_WINDOW,
            )
        }
        .unwrap_or_else(|| peer.endpoint.clone());
        // Explain the state using only the evidence available. A handshake is
        // never described as "connected" and a stale link says how long the peer
        // has been silent.
        let detail = match self.language {
            Language::English => match state {
                LinkState::Disabled => "offline".to_string(),
                LinkState::Connecting if peer.has_outgoing => {
                    format!("link to {display_endpoint} up — waiting for a reply")
                }
                LinkState::Connecting => format!("trying {display_endpoint} — no answer yet"),
                LinkState::Reconnecting => {
                    format!("no reply for {stale_secs}s — reconnecting…{link_metrics}{last_pong}")
                }
                LinkState::ConnectedDegraded => {
                    format!("link up{link_metrics}{last_pong} — emulation disabled on the other PC")
                }
                LinkState::Connected => format!("link healthy{link_metrics}{last_pong}"),
                LinkState::ConnectedBidirectional => {
                    format!("link healthy (bidirectional){link_metrics}{last_pong}")
                }
                LinkState::Receiving if peer.active => {
                    "incoming session — outgoing connection still being established".to_string()
                }
                LinkState::Receiving => {
                    "incoming session — input is applied only while it enters this screen"
                        .to_string()
                }
            },
            Language::PortugueseBrazil => match state {
                LinkState::Disabled => "offline".to_string(),
                LinkState::Connecting if peer.has_outgoing => {
                    format!("link para {display_endpoint} ativo — aguardando resposta")
                }
                LinkState::Connecting => {
                    format!("tentando {display_endpoint} — sem resposta ainda")
                }
                LinkState::Reconnecting => {
                    format!(
                        "sem resposta há {stale_secs}s — reconectando…{link_metrics}{last_pong}"
                    )
                }
                LinkState::ConnectedDegraded => {
                    format!("link ativo{link_metrics}{last_pong} — emulação desativada no outro PC")
                }
                LinkState::Connected => format!("link saudável{link_metrics}{last_pong}"),
                LinkState::ConnectedBidirectional => {
                    format!("link saudável (bidirecional){link_metrics}{last_pong}")
                }
                LinkState::Receiving if peer.active => {
                    "sessão de entrada — conexão de saída ainda sendo estabelecida".to_string()
                }
                LinkState::Receiving => {
                    "sessão de entrada — o input só é aplicado enquanto entra nesta tela"
                        .to_string()
                }
            },
        };
        let (state_color, state_bg) = if state.is_degraded() {
            (AMBER, 0x342b12)
        } else if state == LinkState::Disabled {
            (MUTED, 0x1d222a)
        } else if state == LinkState::Receiving {
            (ACCENT, 0x1b2a3a)
        } else {
            (GREEN, 0x173225)
        };
        let position = peer.position;
        let sensitivity = peer.sensitivity;
        // Outgoing sensitivity only matters when this machine may control the
        // peer. A purely receiving client (the other PC drives this one) would
        // show a control that cannot have any effect.
        let show_sensitivity =
            outgoing_sensitivity_visible(peer.active, state == LinkState::Receiving);
        let primary = peer.primary;
        let action_members = peer.members.clone();
        let edit_members = peer.members.clone();
        let position_members = peer.members.clone();
        let sensitivity_down_members = peer.members.clone();
        let sensitivity_up_members = peer.members.clone();
        let delete_members = peer.members.clone();
        div()
            .flex()
            .flex_col()
            .w_full()
            .overflow_hidden()
            .gap_3()
            .p_4()
            .rounded_lg()
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(TEXT))
                                    .truncate()
                                    .child(peer.name.clone()),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(MUTED))
                                    .truncate()
                                    .child(display_endpoint.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .px_2()
                            .py_1()
                            .rounded_full()
                            .bg(rgb(state_bg))
                            .text_color(rgb(state_color))
                            .text_xs()
                            .child(state.badge()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .gap_1()
                    .child(
                        div().flex().items_center().gap_2().child(
                            div()
                                .id(SharedString::from(format!("toggle-{primary}")))
                                .flex_shrink_0()
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .bg(rgb(match action {
                                    RowAction::Connect => 0x1b2a3a,
                                    _ => 0x2a2020,
                                }))
                                .text_color(rgb(match action {
                                    RowAction::Connect => ACCENT,
                                    _ => RED,
                                }))
                                .text_sm()
                                .cursor_pointer()
                                .child(self.tr(action.label()))
                                .on_click(cx.listener(move |this, _, _, _| {
                                    this.apply_row_action(&action_members, action)
                                })),
                        ),
                    )
                    // Long status lines get their own full-width row and are
                    // truncated instead of pushing the card past the window
                    // (resizing used to clip the whole row).
                    .child(
                        div()
                            .w_full()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .truncate()
                            .child(detail),
                    )
                    .when(version_mismatch, |this| {
                        this.child(
                            div()
                                .w_full()
                                .text_xs()
                                .text_color(rgb(AMBER))
                                .truncate()
                                .child(
                                    self.tr("build mismatch with this peer; update both machines"),
                                ),
                        )
                    })
                    .when(peer.last_error.is_some(), |this| {
                        this.child(
                            div()
                                .w_full()
                                .text_xs()
                                .text_color(rgb(RED))
                                .truncate()
                                .child(format!(
                                    "{} {}",
                                    self.tr("last error:"),
                                    peer.last_error.clone().unwrap_or_default()
                                )),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child({
                        // Clearly labelled side selector. Changing the side used
                        // to be hidden behind a button that silently cycled
                        // left -> right -> top -> bottom.
                        let mut sides: Vec<gpui::AnyElement> = Vec::new();
                        for side in SIDES {
                            let side_members = position_members.clone();
                            let is_current = side == position;
                            sides.push(
                                div()
                                    .id(SharedString::from(format!(
                                        "side-{}-{primary}",
                                        position_label(side)
                                    )))
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .bg(rgb(if is_current { 0x1b2a3a } else { 0x1d222a }))
                                    .text_color(rgb(if is_current { ACCENT } else { TEXT }))
                                    .text_xs()
                                    .cursor_pointer()
                                    .child(position_label(side))
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        this.set_position(&side_members, side)
                                    }))
                                    .into_any_element(),
                            );
                        }
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child(self.tr("Side")),
                            )
                            .children(sides)
                    })
                    .child(self.action_button(
                        "edit-client",
                        self.tr("Edit"),
                        0x1d2430,
                        TEXT,
                        cx,
                        move |this, _, window, _| {
                            this.open_client_edit(&edit_members);
                            window.focus(&this.add_focus);
                        },
                    ))
                    .when(show_sensitivity, |this| {
                        this.child(div().text_xs().text_color(rgb(MUTED)).child(
                            match self.language {
                                Language::PortugueseBrazil => {
                                    format!("sensibilidade {sensitivity}%")
                                }
                                Language::English => format!("sensitivity {sensitivity}%"),
                            },
                        ))
                        .child(self.action_button(
                            "sensitivity-down",
                            "-",
                            0x1d222a,
                            TEXT,
                            cx,
                            move |this, _, _, _| {
                                this.adjust_sensitivity(&sensitivity_down_members, sensitivity, -10)
                            },
                        ))
                        .child(self.action_button(
                            "sensitivity-up",
                            "+",
                            0x1d222a,
                            TEXT,
                            cx,
                            move |this, _, _, _| {
                                this.adjust_sensitivity(&sensitivity_up_members, sensitivity, 10)
                            },
                        ))
                    })
                    .child(self.action_button(
                        "delete-client",
                        self.tr("Remove"),
                        0x3a2020,
                        RED,
                        cx,
                        move |this, _, _, _| this.delete_peer(&delete_members),
                    )),
            )
            .child(self.render_metric_sparkline(primary))
            .into_any_element()
    }

    fn render_metric_sparkline(&self, handle: ClientHandle) -> impl IntoElement + use<> {
        let samples = self
            .metric_history
            .get(&handle)
            .cloned()
            .unwrap_or_default();
        let max = samples
            .iter()
            .map(|sample| sample.rtt_ms.max(1))
            .max()
            .unwrap_or(1);
        let mut bars = Vec::new();
        for sample in &samples {
            let height = 4.0 + 24.0 * (sample.rtt_ms as f32 / max as f32);
            let color = if sample.loss_percent > 0 {
                AMBER
            } else if sample.jitter_ms > 25 {
                ACCENT
            } else {
                GREEN
            };
            bars.push(
                div()
                    .w(px(3.0))
                    .h(px(height))
                    .rounded_sm()
                    .bg(rgb(color))
                    .into_any_element(),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(self.tr("RTT history (last samples)")),
            )
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(px(1.0))
                    .h(px(30.0))
                    .children(bars),
            )
    }

    fn render_network_row(
        &self,
        device: DiscoveredDevice,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let name = device
            .hostname
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| device.ip.to_string());
        let port = device.port.unwrap_or(lan_mouse_ipc::DEFAULT_PORT);
        let status = if device.configured {
            self.tr("configured").to_string()
        } else if device.lan_mouse {
            self.tr("Lan Mouse available").to_string()
        } else {
            self.tr("host reachable (Lan Mouse not confirmed)")
                .to_string()
        };
        let version = device
            .version
            .clone()
            .map(|version| format!(" · build {version}"))
            .unwrap_or_default();
        let verification = match device.fingerprint.as_ref() {
            Some(_) if device.fingerprint_verified => self.tr("fingerprint verified"),
            Some(_) => self.tr("fingerprint not verified"),
            None => self.tr("fingerprint unavailable"),
        };
        let verification_color = if device.fingerprint_verified {
            GREEN
        } else {
            AMBER
        };
        let last_seen = device
            .last_seen
            .map(|seen| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis() as u64)
                    .unwrap_or(0);
                format!(" · seen {}s ago", now.saturating_sub(seen) / 1000)
            })
            .unwrap_or_default();
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .p_4()
            .rounded_lg()
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child(name),
                    )
                    .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                        "{}:{} · {status}{version}{last_seen}",
                        device.ip, port
                    )))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(verification_color))
                            .child(verification),
                    ),
            )
            .when(!device.configured, |this| {
                this.child(self.action_button(
                    network_add_id(device.ip, port),
                    self.tr("Add"),
                    ACCENT,
                    0x081018,
                    cx,
                    move |this, _, _, _| {
                        this.add_discovered(device.ip, port, device.fingerprint.clone())
                    },
                ))
            })
    }
}

impl Dashboard {
    fn render_cursor_extension_banner(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_2()
            .px_6()
            .py_2()
            .bg(rgb(0x2a2415))
            .border_b_1()
            .border_color(rgb(0x5a4a20))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child(self.tr("Cursor hiding extension needs a logout")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(self.tr(
                                "The GNOME Shell extension was installed. Log out and back in once to activate it.",
                            )),
                    ),
            )
            .child(self.action_button(
                "cursor-extension-banner-ok",
                self.tr("OK"),
                0x1d2430,
                TEXT,
                cx,
                |this, _, _, cx| {
                    this.cursor_extension.hint = None;
                    cx.notify();
                },
            ))
    }
}

impl Render for Dashboard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rem_size = px(16.0);
        if window.rem_size() != rem_size {
            window.set_rem_size(rem_size);
        }
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            log::info!("GPUI first render");
        }
        let sidebar = div()
            .w(px(220.0))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .gap_6()
            .p_4()
            .bg(rgb(SIDEBAR))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_lg().text_color(rgb(TEXT)).child("Lan Mouse")),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(self.sidebar_item(Page::Devices, self.tr("Devices"), "◉", cx))
                    .child(self.sidebar_item(Page::Network, self.tr("Network"), "⌁", cx))
                    .child(self.sidebar_item(Page::Settings, self.tr("Settings"), "⚙", cx)),
            )
            .child(div().flex_grow())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(self.status.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("{} active link(s)", self.healthy_link_count())),
                    ),
            );

        let content = match self.page {
            Page::Devices => self.render_devices(cx).into_any_element(),
            Page::Network => self.render_network(cx).into_any_element(),
            Page::Settings => self.render_settings(cx).into_any_element(),
        };

        // Explicit height is required for overflow_y_scroll: without a bounded
        // parent the page grows to its content and is clipped by the window.
        let chrome_height = if cfg!(target_os = "linux") {
            34.0 + 64.0
        } else {
            64.0
        };
        let content_height = (window.viewport_size().height - px(chrome_height)).max(px(120.0));

        let shell = div().flex().flex_grow().child(sidebar).child(
            div()
                .flex()
                .flex_col()
                .flex_grow()
                .child(
                    div()
                        .h(px(64.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_6()
                        .border_b_1()
                        .border_color(rgb(BORDER))
                        .child(
                            div()
                                .text_lg()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(rgb(TEXT))
                                .child(match self.page {
                                    Page::Devices => self.tr("Devices"),
                                    Page::Network => self.tr("Network"),
                                    Page::Settings => self.tr("Settings"),
                                }),
                        )
                        .when(self.page == Page::Devices, |this| {
                            this.child(
                                div()
                                    .id("add-device")
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .bg(rgb(ACCENT))
                                    .text_color(rgb(0x081018))
                                    .text_sm()
                                    .cursor_pointer()
                                    .child(self.tr("+ Add device"))
                                    .on_click(cx.listener(|this, _, window, _| {
                                        this.add_device();
                                        window.focus(&this.add_focus);
                                    })),
                            )
                        })
                        .when(self.page == Page::Network, |this| {
                            this.child(
                                div()
                                    .id("scan-network-header")
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .bg(rgb(ACCENT))
                                    .text_color(rgb(0x081018))
                                    .text_sm()
                                    .cursor_pointer()
                                    .child(self.tr("Scan network"))
                                    .on_click(cx.listener(|this, _, _, _| this.start_scan())),
                            )
                        }),
                )
                .when(self.settings_dialog.is_some(), |this| {
                    this.child(self.render_settings_dialog(cx))
                })
                .when(self.cursor_extension.hint.is_some(), |this| {
                    this.child(self.render_cursor_extension_banner(cx))
                })
                .child(
                    div()
                        .w_full()
                        .h(content_height)
                        .flex_none()
                        .overflow_hidden()
                        .child(content),
                ),
        );

        div()
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(rgb(BG))
            .when(cfg!(target_os = "linux"), |this| {
                this.child(self.render_titlebar(window, cx))
            })
            .child(shell)
    }
}

impl Dashboard {
    fn render_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let maximized = window.is_maximized();
        div()
            .h(px(34.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .bg(rgb(0x0b0d11))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .id("titlebar-drag")
                    .flex()
                    .flex_grow()
                    .h_full()
                    .items_center()
                    .pl_3()
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, window, _| {
                            window.start_window_move();
                        }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child("Lan Mouse"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .id("window-minimize")
                            .w(px(36.0))
                            .h(px(34.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(0x202631)))
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child("—")
                            .on_click(cx.listener(|_, _, window, _| window.minimize_window())),
                    )
                    .child(
                        div()
                            .id("window-zoom")
                            .w(px(36.0))
                            .h(px(34.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(0x202631)))
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child(if maximized { "❐" } else { "□" })
                            .on_click(cx.listener(|_, _, window, _| window.zoom_window())),
                    )
                    .child(
                        div()
                            .id("window-close")
                            .w(px(36.0))
                            .h(px(34.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(0xc42b1c)))
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child("✕")
                            // Close the window but keep the process (and the
                            // LAN connection) alive: the tray "Open" re-creates
                            // it. "Quit Lan Mouse" is what stops everything.
                            .on_click(cx.listener(|_, _, window, _| window.remove_window())),
                    ),
            )
    }

    fn action_button(
        &self,
        id: impl Into<SharedString>,
        label: &'static str,
        background: u32,
        foreground: u32,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &gpui::ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id.into())
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(background))
            .text_color(rgb(foreground))
            .text_sm()
            .cursor_pointer()
            .child(label)
            .on_click(cx.listener(handler))
    }

    fn render_settings_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let Some(dialog) = self.settings_dialog.clone() else {
            return div().into_any_element();
        };
        let focus = self.add_focus.clone();
        let (title, hint, fields) = match dialog.kind {
            SettingsDialogKind::Address => (
                self.tr("Add allowed address"),
                self.tr("IPv4 address or CIDR. Tab switches fields, Enter saves, Esc cancels."),
                vec![("address", dialog.address.clone())],
            ),
            SettingsDialogKind::Certificate => (
                self.tr("Authorize certificate"),
                self.tr("Fingerprint and display name of the peer. Tab switches fields."),
                vec![
                    ("name", dialog.name.clone()),
                    ("fingerprint", dialog.fingerprint.clone()),
                ],
            ),
            SettingsDialogKind::Client { .. } => (
                self.tr("Edit client"),
                self.tr("Hostname/IP and port. Leave the hostname empty to use the address."),
                vec![
                    ("hostname / IP", dialog.hostname.clone()),
                    ("port", dialog.port.clone()),
                ],
            ),
            SettingsDialogKind::RenameDevice { .. } => (
                self.tr("Rename device"),
                self.tr("Display name only; the certificate fingerprint stays the identity."),
                vec![("display name", dialog.name.clone())],
            ),
            SettingsDialogKind::Port => (
                self.tr("Change listening port"),
                self.tr(
                    "The UDP listener is recreated on this port. Peers must use the same port.",
                ),
                vec![("port", dialog.port.clone())],
            ),
        };
        let mut field_rows = Vec::new();
        for (idx, (label, value)) in fields.clone().into_iter().enumerate() {
            let active = dialog.field as usize == idx;
            let row = div()
                .id(SharedString::from(format!("settings-field-{idx}")))
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded_md()
                .bg(rgb(if active { 0x1d2430 } else { 0x121826 }))
                .cursor_pointer()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(if active { ACCENT } else { MUTED }))
                        .child(self.tr(label)),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(TEXT))
                        .child(format!("{value}▌")),
                )
                .on_click(cx.listener(move |this, _, window, _| {
                    if let Some(dialog) = this.settings_dialog.as_mut() {
                        dialog.field = idx as u8;
                    }
                    window.focus(&this.add_focus);
                }));
            field_rows.push(row);
        }
        div()
            .id("settings-dialog")
            .track_focus(&focus)
            .on_key_down(cx.listener(Self::on_settings_key))
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded_lg()
            .bg(rgb(0x121826))
            .border_1()
            .border_color(rgb(ACCENT))
            .child(
                div()
                    .text_base()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(TEXT))
                    .child(title),
            )
            .child(div().text_xs().text_color(rgb(MUTED)).child(hint))
            .children(field_rows)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.action_button(
                        "settings-dialog-submit",
                        self.tr("Save"),
                        ACCENT,
                        0x081018,
                        cx,
                        |this, _, _, _| this.submit_settings_dialog(),
                    ))
                    .child(self.action_button(
                        "settings-dialog-cancel",
                        self.tr("Cancel"),
                        0x1d222a,
                        TEXT,
                        cx,
                        |this, _, _, _| this.close_settings_dialog(),
                    )),
            )
            .into_any_element()
    }

    fn render_add_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let Some(dialog) = self.add_dialog.clone() else {
            return div().into_any_element();
        };
        let focus = self.add_focus.clone();
        let host_focus = focus.clone();
        let host_active = dialog.field == AddField::Hostname;
        let port_active = dialog.field == AddField::Port;
        let fp_active = dialog.field == AddField::Fingerprint;
        div()
            .id("add-device-dialog")
            .track_focus(&focus)
            .on_key_down(cx.listener(Self::on_add_key))
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded_lg()
            .bg(rgb(0x121826))
            .border_1()
            .border_color(rgb(ACCENT))
            .child(
                div()
                    .text_base()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(TEXT))
                    .child(self.tr("Add device")),
            )
            .child(div().text_xs().text_color(rgb(MUTED)).child(self.tr(
                "Type a hostname or IPv4 address. Tab switches fields, Enter adds, Esc cancels.",
            )))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(self.action_button(
                        "add-field-host",
                        self.tr("Host"),
                        0x1d2430,
                        if host_active { ACCENT } else { TEXT },
                        cx,
                        move |this, _, window, _| {
                            if let Some(dialog) = this.add_dialog.as_mut() {
                                dialog.field = AddField::Hostname;
                            }
                            window.focus(&host_focus);
                        },
                    ))
                    .child(self.action_button(
                        "add-field-port",
                        self.tr("Port"),
                        0x1d2430,
                        if port_active { ACCENT } else { TEXT },
                        cx,
                        |this, _, window, _| {
                            if let Some(dialog) = this.add_dialog.as_mut() {
                                dialog.field = AddField::Port;
                            }
                            window.focus(&this.add_focus);
                        },
                    ))
                    .child(self.action_button(
                        "add-field-fingerprint",
                        self.tr("Fingerprint"),
                        0x1d2430,
                        if fp_active { ACCENT } else { TEXT },
                        cx,
                        |this, _, window, _| {
                            if let Some(dialog) = this.add_dialog.as_mut() {
                                dialog.field = AddField::Fingerprint;
                            }
                            window.focus(&this.add_focus);
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(if host_active { ACCENT } else { MUTED }))
                            .child(self.tr("hostname / IP")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child(format!("{}▌", dialog.hostname)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(if port_active { ACCENT } else { MUTED }))
                            .child(self.tr("port")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child(format!("{}▌", dialog.port)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(if fp_active { ACCENT } else { MUTED }))
                            .child("fingerprint (optional, advanced)"),
                    )
                    .child(div().text_sm().text_color(rgb(TEXT)).child(
                        if dialog.fingerprint.is_empty() {
                            "—".to_string()
                        } else {
                            format!("{}▌", dialog.fingerprint)
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("peer is on the"),
                    )
                    .child(self.action_button(
                        "add-pos-left",
                        "Left",
                        0x1d2430,
                        TEXT,
                        cx,
                        |this, _, _, _| {
                            if let Some(dialog) = this.add_dialog.as_mut() {
                                dialog.position = Position::Left;
                            }
                        },
                    ))
                    .child(self.action_button(
                        "add-pos-right",
                        "Right",
                        0x1d2430,
                        TEXT,
                        cx,
                        |this, _, _, _| {
                            if let Some(dialog) = this.add_dialog.as_mut() {
                                dialog.position = Position::Right;
                            }
                        },
                    ))
                    .child(self.action_button(
                        "add-pos-top",
                        "Top",
                        0x1d2430,
                        TEXT,
                        cx,
                        |this, _, _, _| {
                            if let Some(dialog) = this.add_dialog.as_mut() {
                                dialog.position = Position::Top;
                            }
                        },
                    ))
                    .child(self.action_button(
                        "add-pos-bottom",
                        "Bottom",
                        0x1d2430,
                        TEXT,
                        cx,
                        |this, _, _, _| {
                            if let Some(dialog) = this.add_dialog.as_mut() {
                                dialog.position = Position::Bottom;
                            }
                        },
                    ))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(ACCENT))
                            .child(position_label(dialog.position)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.action_button(
                        "add-dialog-submit",
                        "Add device",
                        ACCENT,
                        0x081018,
                        cx,
                        |this, _, _, _| this.submit_add_dialog(),
                    ))
                    .child(self.action_button(
                        "add-dialog-cancel",
                        self.tr("Cancel"),
                        0x1d222a,
                        TEXT,
                        cx,
                        |this, _, _, _| this.close_add_dialog(),
                    )),
            )
            .into_any_element()
    }

    fn render_identity_card(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let ips = if self.local_ips.is_empty() {
            "no IPv4 address".to_string()
        } else {
            self.local_ips.join(" · ")
        };
        let ips_for_copy = self
            .local_ips
            .first()
            .cloned()
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let self_port = self.port;
        let fingerprint_full = self
            .public_fingerprint
            .clone()
            .unwrap_or_else(|| "fingerprint not received".to_string());
        let fingerprint = if self.public_fingerprint.is_some() {
            short_fingerprint(&fingerprint_full, 16)
        } else {
            fingerprint_full.clone()
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .rounded_lg()
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_base()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child(format!("{} ({})", self.hostname, self.tr("this computer"))),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_full()
                            .bg(rgb(0x173225))
                            .text_color(rgb(GREEN))
                            .text_xs()
                            .child(format!("{} {}", self.tr("port"), self.port)),
                    ),
            )
            .child(div().text_sm().text_color(rgb(MUTED)).child(ips))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(format!("{} {fingerprint}", self.tr("certificate"))),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.action_button(
                        "copy-address",
                        self.tr("Copy address"),
                        0x1d2430,
                        TEXT,
                        cx,
                        move |_, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(format!(
                                "{ips_for_copy}:{self_port}"
                            )));
                        },
                    ))
                    .child(self.action_button(
                        "copy-fingerprint",
                        self.tr("Copy fingerprint"),
                        0x1d2430,
                        TEXT,
                        cx,
                        move |_, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                fingerprint_full.clone(),
                            ));
                        },
                    )),
            )
    }

    fn render_pending(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let pending = self.pending.clone();
        let mut cards = Vec::new();
        for request in pending {
            let approve_fp = request.fingerprint.clone();
            let deny_fp = request.fingerprint.clone();
            let cert_changed = request.previous_fingerprint.is_some();
            let cert_warning = request.previous_fingerprint.as_deref().map(|previous| {
                format!(
                    "⚠ certificate changed (old: {}). Only approve if you replaced it; the old identity will be revoked.",
                    short_fingerprint(previous, 20)
                )
            });
            let name = request
                .name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| request.addr.to_string());
            let sas = if request.sas.is_empty() {
                "······".to_string()
            } else {
                request.sas.clone()
            };
            let card = div()
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .rounded_lg()
                .bg(rgb(0x2b2413))
                .border_1()
                .border_color(rgb(0x6b5520))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(rgb(AMBER))
                                .child(format!("Connection request from {name}")),
                        )
                        .child(
                            div()
                                .text_lg()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(rgb(TEXT))
                                .child(sas),
                        ),
                )
                .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                    "{} · {} attempt(s)",
                    request.addr, request.attempts
                )))
                .when(cert_warning.is_some(), move |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(rgb(RED))
                            .child(cert_warning.clone().unwrap_or_default()),
                    )
                })
                .child(
                    div()
                        .flex()
                        .gap_2()
                        // Accepting a request always grants both directions
                        // (full pointer/keyboard/clipboard in and out); the
                        // per-direction presets stay in the device permissions
                        // panel for later adjustments.
                        .child(self.action_button(
                            "approve-pending",
                            if cert_changed {
                                self.tr("Trust new certificate")
                            } else {
                                self.tr("Approve")
                            },
                            GREEN,
                            0x081018,
                            cx,
                            move |this, _, _, _| this.approve_pending(approve_fp.clone()),
                        ))
                        .child(self.action_button(
                            "deny-pending",
                            self.tr("Deny"),
                            0x3a2020,
                            RED,
                            cx,
                            move |this, _, _, _| this.deny_pending(deny_fp.clone()),
                        )),
                );
            cards.push(card);
        }
        div().flex().flex_col().gap_2().children(cards)
    }

    fn render_devices(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let groups = self.peer_groups();
        let mut cards = Vec::with_capacity(groups.len());
        for members in &groups {
            cards.push(self.render_peer(members, cx));
        }

        let pending = self.pending.len();
        let has_pending = pending > 0;
        div()
            .id("devices-page")
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_6()
            .overflow_y_scroll()
            .child(
                div().flex().items_center().justify_between().child(
                    div()
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .child(match self.language {
                            Language::PortugueseBrazil => format!(
                                "{} dispositivo(s) configurado(s){}",
                                self.clients.len(),
                                if pending > 0 {
                                    format!(" · {pending} pedido(s) pendente(s)")
                                } else {
                                    String::new()
                                }
                            ),
                            Language::English => format!(
                                "{} configured device(s){}",
                                self.clients.len(),
                                if pending > 0 {
                                    format!(" · {pending} pending request(s)")
                                } else {
                                    String::new()
                                }
                            ),
                        }),
                ),
            )
            .when(self.add_dialog.is_some(), |this| {
                this.child(self.render_add_dialog(cx))
            })
            .child(self.render_identity_card(cx))
            .when(has_pending, |this| this.child(self.render_pending(cx)))
            .children(cards)
            .when(self.clients.is_empty(), |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .py_16()
                        .child(
                            div()
                                .text_2xl()
                                .text_color(rgb(MUTED))
                                .child(self.tr("No devices yet")),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(MUTED))
                                .child("Add another computer to start sharing input."),
                        ),
                )
            })
    }

    fn render_network(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        div()
            .id("network-page")
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_6()
            .overflow_y_scroll()
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child(self.tr("Find Lan Mouse instances on the local IPv4 network.")),
            )
            .when(self.discovered.is_empty(), |this| {
                this.child(
                    div()
                        .p_6()
                        .rounded_lg()
                        .bg(rgb(CARD))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(MUTED))
                                .child(self.tr("No devices discovered yet. Press Scan network.")),
                        ),
                )
            })
            .child(
                div().flex().flex_col().gap_2().children(
                    self.discovered
                        .clone()
                        .into_iter()
                        .map(|device| self.render_network_row(device, cx).into_any_element())
                        .collect::<Vec<_>>(),
                ),
            )
    }

    fn render_pairing_review(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut rows = Vec::new();
        for device in self.pairing_review.clone() {
            let keep_fp = device.fingerprint.clone();
            let revoke_fp = device.fingerprint.clone();
            rows.push(
                div()
                    .id(SharedString::from(format!(
                        "pair-review-{}",
                        device.fingerprint
                    )))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_sm().text_color(rgb(TEXT)).child(device.name))
                            .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                                "{} · authorized via {}",
                                short_fingerprint(&device.fingerprint, 20),
                                device.source
                            ))),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(self.action_button(
                                "pairing-keep",
                                self.tr("Keep"),
                                GREEN,
                                0x081018,
                                cx,
                                move |this, _, _, _| this.dismiss_pairing_review(keep_fp.clone()),
                            ))
                            .child(self.action_button(
                                "pairing-revoke",
                                self.tr("Revoke"),
                                0x3a2020,
                                RED,
                                cx,
                                move |this, _, _, _| {
                                    this.revoke_device(revoke_fp.clone());
                                    this.dismiss_pairing_review(revoke_fp.clone());
                                },
                            )),
                    ),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .rounded_lg()
            .bg(rgb(0x2b2413))
            .border_1()
            .border_color(rgb(0x6b5520))
            .child(
                div()
                    .text_base()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(AMBER))
                    .child(format!("{} ({})", self.tr("Pairing review"), rows.len())),
            )
            .child(div().text_xs().text_color(rgb(MUTED)).child(self.tr(
                self.tr("Devices adopted while the pairing window was open. Keep them or revoke access."),
            )))
            .children(rows)
    }

    /// Install/enable the bundled GNOME cursor-hiding extension.
    fn install_cursor_extension(&mut self, cx: &mut Context<Self>) {
        match cursor_extension::install() {
            Ok(state) => {
                self.cursor_extension = state;
                self.status = self
                    .tr("Extension installed. Log out and back in to activate it.")
                    .to_string();
            }
            Err(error) => {
                self.status = format!("{}: {error}", self.tr("Extension installation failed"));
            }
        }
        cx.notify();
    }

    fn render_settings_general(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let local_commit = String::from_utf8_lossy(&self.local_commit)
            .trim_matches('\0')
            .to_string();
        let ips = if self.local_ips.is_empty() {
            "no IPv4 address".to_string()
        } else {
            self.local_ips.join(" · ")
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .rounded_lg()
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .text_base()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(TEXT))
                    .child(self.tr("General")),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child(self.tr("Language")),
                    )
                    .child(self.action_button(
                        "language-en",
                        "English",
                        if self.language == Language::English {
                            ACCENT
                        } else {
                            0x1d2430
                        },
                        if self.language == Language::English {
                            0x081018
                        } else {
                            TEXT
                        },
                        cx,
                        |this, _, _, cx| this.set_language(Language::English, cx),
                    ))
                    .child(self.action_button(
                        "language-ptbr",
                        "Português (Brasil)",
                        if self.language == Language::PortugueseBrazil {
                            ACCENT
                        } else {
                            0x1d2430
                        },
                        if self.language == Language::PortugueseBrazil {
                            0x081018
                        } else {
                            TEXT
                        },
                        cx,
                        |this, _, _, cx| this.set_language(Language::PortugueseBrazil, cx),
                    )),
            )
            .when(self.cursor_extension.needed, |this| {
                let extension = self.cursor_extension.clone();
                let (label, hint) = if extension.enabled {
                    (
                        self.tr("Cursor hiding extension active"),
                        self.tr("Log out and back in once if the cursor is still visible."),
                    )
                } else if extension.installed {
                    (
                        self.tr("Cursor hiding extension installed but not enabled"),
                        self.tr("Enable it and log out/in once to hide the cursor on this PC."),
                    )
                } else {
                    (
                        self.tr("Hide the local cursor on GNOME Wayland"),
                        self.tr(
                            "Installs the optional GNOME Shell extension. Wayland loads it after a logout/login.",
                        ),
                    )
                };
                let button = if extension.enabled {
                    self.tr("Reinstall extension")
                } else if extension.installed {
                    self.tr("Enable extension")
                } else {
                    self.tr("Install extension")
                };
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(rgb(TEXT))
                                        .child(self.tr("Local cursor while controlling another PC")),
                                )
                                .child(self.action_button(
                                    "install-cursor-extension",
                                    button,
                                    0x1d2430,
                                    TEXT,
                                    cx,
                                    |this, _, _, cx| this.install_cursor_extension(cx),
                                )),
                        )
                        .child(div().text_xs().text_color(rgb(MUTED)).child(label))
                        .child(div().text_xs().text_color(rgb(MUTED)).child(hint)),
                )
            })
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(TEXT))
                    .child(self.hostname.clone()),
            )
            .child(div().text_xs().text_color(rgb(MUTED)).child(ips))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .w_full()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child(format!(
                                "listening port {} · build {}",
                                self.port, local_commit
                            )),
                    )
                    .child(self.action_button(
                        "change-port",
                        self.tr("Change port"),
                        0x1d2430,
                        TEXT,
                        cx,
                        |this, _, window, _| {
                            this.open_port_dialog();
                            window.focus(&this.add_focus);
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(TEXT))
                                    .child(self.tr("Cursor speed while controlled remotely")),
                            )
                            .child(self.action_button(
                                "abs-sensitivity-down",
                                "-",
                                0x1d2430,
                                TEXT,
                                cx,
                                |this, _, _, _| {
                                    let next = this.absolute_sensitivity.saturating_sub(10).max(10);
                                    this.send(FrontendRequest::SetAbsoluteSensitivity(next));
                                },
                            ))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(TEXT))
                                    .child(format!("{}%", self.absolute_sensitivity)),
                            )
                            .child(self.action_button(
                                "abs-sensitivity-up",
                                "+",
                                0x1d2430,
                                TEXT,
                                cx,
                                |this, _, _, _| {
                                    let next = (this.absolute_sensitivity + 10).min(500);
                                    this.send(FrontendRequest::SetAbsoluteSensitivity(next));
                                },
                            )),
                    )
                    .child(div().w_full().text_xs().text_color(rgb(MUTED)).child(
                        self.tr("applies when another PC controls this computer; 100% = 1:1"),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(self.action_button(
                        "save-config",
                        self.tr("Save configuration"),
                        ACCENT,
                        0x081018,
                        cx,
                        |this, _, _, _| this.send(FrontendRequest::SaveConfiguration),
                    ))
                    .child(self.action_button(
                        "refresh-history",
                        self.tr("Refresh history"),
                        0x1d2430,
                        TEXT,
                        cx,
                        |this, _, _, _| this.send(FrontendRequest::ListHistory { limit: 20 }),
                    ))
                    .child(self.action_button(
                        "quit-app",
                        self.tr("Quit Lan Mouse"),
                        0x3a2020,
                        RED,
                        cx,
                        // Closing the window only closes the UI; this is the
                        // explicit "stop everything" action. Ask the service to
                        // shut down over IPC first (it may have been started by
                        // systemd or by an earlier launch, so the launcher may
                        // not own it), then quit the UI process.
                        |this, _, _, cx| {
                            this.send(FrontendRequest::Shutdown);
                            request_quit();
                            cx.quit();
                        },
                    )),
            )
    }

    fn render_settings_advanced(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let capture = self
            .diagnostics
            .capture_backend
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let emulation = self
            .diagnostics
            .emulation_backend
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let session = self
            .diagnostics
            .session_type
            .clone()
            .unwrap_or_else(|| self.tr("unknown").to_string());
        let desktop = self
            .diagnostics
            .desktop
            .clone()
            .unwrap_or_else(|| self.tr("unknown").to_string());
        let clipboard_backend = self
            .diagnostics
            .clipboard_backend
            .clone()
            .unwrap_or_else(|| self.tr("unavailable").to_string());
        let mut history_rows = Vec::new();
        let now_ms = unix_ms_now();
        for entry in self.history.iter().take(8) {
            let when = format_timestamp_age(entry.ts, now_ms);
            let subject = entry
                .name
                .clone()
                .or_else(|| {
                    entry
                        .fingerprint
                        .clone()
                        .map(|fp| short_fingerprint(&fp, 16))
                })
                .unwrap_or_else(|| "—".to_string());
            let detail = entry
                .detail
                .clone()
                .or_else(|| entry.addr.clone())
                .unwrap_or_default();
            history_rows.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child(format!("{} · {subject}", entry.event)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("{when} {detail}")),
                    ),
            );
        }
        let exported = self
            .policy_export
            .as_ref()
            .map(|json| match self.language {
                Language::PortugueseBrazil => format!("{} bytes prontos", json.len()),
                Language::English => format!("{} bytes ready", json.len()),
            })
            .unwrap_or_else(|| self.tr("not exported yet").to_string());
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .rounded_lg()
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .text_base()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(TEXT))
                    .child(self.tr("Diagnostics & history")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(match self.language {
                        Language::PortugueseBrazil => format!(
                            "captura: {capture} · emulação: {emulation} · sessão: {session} · desktop: {desktop} · clipboard: {clipboard_backend}"
                        ),
                        Language::English => format!(
                            "capture: {capture} · emulation: {emulation} · session: {session} · desktop: {desktop} · clipboard: {clipboard_backend}"
                        ),
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(match self.language {
                        Language::PortugueseBrazil => format!(
                            "protocolo v{} · capacidades {:#x} · porta {}",
                            self.diagnostics.proto_version,
                            self.diagnostics.capabilities,
                            self.diagnostics.port
                        ),
                        Language::English => format!(
                            "protocol v{} · capabilities {:#x} · port {}",
                            self.diagnostics.proto_version,
                            self.diagnostics.capabilities,
                            self.diagnostics.port
                        ),
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.action_button(
                        "export-policy",
                        self.tr("Export policy"),
                        0x1d2430,
                        TEXT,
                        cx,
                        |this, _, _, _| {
                            this.send(FrontendRequest::ExportPolicy);
                        },
                    ))
                    .child(self.action_button(
                        "copy-policy",
                        self.tr("Copy exported"),
                        0x1d2430,
                        TEXT,
                        cx,
                        |this, _, _, cx| {
                            if let Some(policy) = this.policy_export.clone() {
                                cx.write_to_clipboard(ClipboardItem::new_string(policy));
                            }
                        },
                    )),
            )
            .child(div().text_xs().text_color(rgb(MUTED)).child(exported))
            .when(!history_rows.is_empty(), |this| this.children(history_rows))
    }

    fn render_settings_about(&self) -> impl IntoElement + use<> {
        let commit = String::from_utf8_lossy(&self.local_commit)
            .trim_matches('\0')
            .to_string();
        let platform = if cfg!(target_os = "windows") {
            "Windows"
        } else {
            "Linux"
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .rounded_lg()
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .text_base()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(TEXT))
                    .child(self.tr("About")),
            )
            .child(div().text_sm().text_color(rgb(TEXT)).child(
                self.tr("Lan Mouse — unofficial fork · GPUI frontend · GPL-3.0-or-later"),
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(match self.language {
                        Language::PortugueseBrazil => format!(
                            "compilação {commit} · interface {} · {platform}",
                            env!("CARGO_PKG_VERSION")
                        ),
                        Language::English => format!(
                            "build {commit} · frontend {} · {platform}",
                            env!("CARGO_PKG_VERSION")
                        ),
                    }),
            )
            .child(div().text_xs().text_color(rgb(MUTED)).child(
                self.tr("The GTK frontend is still available as an opt-in fallback; the IPC/service protocol is unchanged."),
            ))
    }

    fn render_device_permissions(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let devices = self.devices.clone();
        let mut rows = Vec::new();
        for device in devices {
            let fp = device.fingerprint.clone();
            let deny_fp = fp.clone();
            let rename_fp = fp.clone();
            let rename_name = device.name.clone();
            let revoke_fp = fp;
            let row = div()
                .flex()
                .flex_col()
                .gap_2()
                .p_4()
                .rounded_lg()
                .bg(rgb(CARD))
                .border_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(rgb(TEXT))
                                        .child(device.name.clone()),
                                )
                                .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                                    "{} · {} · {}",
                                    short_fingerprint(&device.fingerprint, 16),
                                    device.incoming.capabilities.summary(),
                                    device.source
                                ))),
                        )
                        .child({
                            // `DeviceView::connected` only proves that the service
                            // holds a session/optimistic flag, not that the peer
                            // answers: only a fresh Pong may claim "connected".
                            let verified =
                                has_fresh_pong(&self.clients, &device.fingerprint, unix_ms_now());
                            let (label, color, background) = if verified {
                                (self.tr("connected"), GREEN, 0x173225u32)
                            } else if device.connected {
                                (self.tr("unconfirmed"), AMBER, 0x342b12u32)
                            } else {
                                (self.tr("offline"), MUTED, 0x1d222au32)
                            };
                            div()
                                .px_2()
                                .py_1()
                                .rounded_full()
                                .bg(rgb(background))
                                .text_color(rgb(color))
                                .text_xs()
                                .child(label)
                        }),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(self.action_button(
                            "policy-none",
                            self.tr("Block"),
                            0x3a2020,
                            RED,
                            cx,
                            move |this, _, _, _| {
                                this.set_policy(
                                    deny_fp.clone(),
                                    DirectionPolicy::deny(),
                                    DirectionPolicy::deny(),
                                )
                            },
                        ))
                        .child(self.action_button(
                            "rename-device",
                            self.tr("Rename"),
                            0x1d2430,
                            TEXT,
                            cx,
                            move |this, _, window, _| {
                                this.open_device_rename(rename_fp.clone(), rename_name.clone());
                                window.focus(&this.add_focus);
                            },
                        ))
                        .child(self.action_button(
                            "revoke-device",
                            self.tr("Revoke"),
                            0x3a2020,
                            RED,
                            cx,
                            move |this, _, _, _| this.revoke_device(revoke_fp.clone()),
                        )),
                );
            rows.push(row);
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_base()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(TEXT))
                    .child(format!("{} ({})", self.tr("Devices"), rows.len())),
            )
            .when(rows.is_empty(), |this| {
                this.child(
                    div()
                        .p_4()
                        .rounded_lg()
                        .bg(rgb(CARD))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(MUTED))
                                .child(self.tr("No known devices.")),
                        ),
                )
            })
            .children(rows)
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let capture_label = if matches!(self.capture_status, Status::Enabled) {
            self.tr("capture enabled")
        } else {
            self.tr("capture disabled")
        };
        let emulation_label = if matches!(self.emulation_status, Status::Enabled) {
            self.tr("emulation enabled")
        } else {
            self.tr("emulation disabled")
        };
        let clipboard_summary = if !self.clipboard_status.enabled {
            self.tr("disabled").to_string()
        } else if !self.clipboard_status.available {
            self.tr("backend unavailable").to_string()
        } else {
            let backend = self
                .clipboard_status
                .backend
                .as_deref()
                .unwrap_or("unknown backend");
            match self.language {
                Language::PortugueseBrazil => format!(
                    "{backend} · {} recebidos · {} locais",
                    self.clipboard_status.applied, self.clipboard_status.detected
                ),
                Language::English => format!(
                    "{backend} · {} received · {} local",
                    self.clipboard_status.applied, self.clipboard_status.detected
                ),
            }
        };
        let pairing_label = if self.pairing_active {
            self.tr("pairing window open")
        } else {
            self.tr("open a 60 second pairing window")
        };
        let mut authorized = self
            .authorized
            .iter()
            .map(|(fingerprint, name)| format!("{name} · {}", short_fingerprint(fingerprint, 16)))
            .collect::<Vec<_>>();
        authorized.sort();
        let mut rules = self
            .address_rules
            .iter()
            .map(|rule| {
                format!(
                    "{} · in:{} out:{}",
                    rule.address,
                    rule.incoming.capabilities.summary(),
                    rule.outgoing.capabilities.summary()
                )
            })
            .collect::<Vec<_>>();
        rules.sort();

        let enabled_color = if matches!(self.capture_status, Status::Enabled) {
            GREEN
        } else {
            AMBER
        };
        let emulation_color = if matches!(self.emulation_status, Status::Enabled) {
            GREEN
        } else {
            AMBER
        };
        div()
            .id("settings-page")
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .overflow_y_scroll()
            .child(self.render_settings_general(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(CARD))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_base()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child(self.tr("Input")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(enabled_color))
                                    .child(capture_label),
                            )
                            .child(self.action_button(
                                "enable-capture",
                                self.tr("Reenable"),
                                ACCENT,
                                0x081018,
                                cx,
                                |this, _, _, _| this.enable_capture(),
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(emulation_color))
                                    .child(emulation_label),
                            )
                            .child(self.action_button(
                                "enable-emulation",
                                self.tr("Reenable"),
                                ACCENT,
                                0x081018,
                                cx,
                                |this, _, _, _| this.enable_emulation(),
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(CARD))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_base()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child(self.tr("Clipboard")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(MUTED))
                                    .child(clipboard_summary),
                            )
                            .child(self.action_button(
                                "toggle-clipboard",
                                if self.clipboard_status.enabled {
                                    self.tr("Disable")
                                } else {
                                    self.tr("Enable")
                                },
                                ACCENT,
                                0x081018,
                                cx,
                                |this, _, _, _| this.toggle_clipboard(),
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(CARD))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_base()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child(self.tr("Pairing")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().text_sm().text_color(rgb(MUTED)).child(pairing_label))
                            .when(self.pairing_active, |this| {
                                // The service owns the pairing timer: keep the
                                // button non-interactive while a window is open so
                                // clicks cannot queue duplicate pairing_open
                                // records in the audit log.
                                this.child(
                                    div()
                                        .px_3()
                                        .py_2()
                                        .rounded_md()
                                        .bg(rgb(0x1d222a))
                                        .text_color(rgb(MUTED))
                                        .text_sm()
                                        .child(self.tr("Pairing…")),
                                )
                            })
                            .when(!self.pairing_active, |this| {
                                this.child(self.action_button(
                                    "start-pairing",
                                    self.tr("Open"),
                                    ACCENT,
                                    0x081018,
                                    cx,
                                    |this, _, _, _| this.start_pairing(),
                                ))
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(CARD))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(TEXT))
                                    .child(format!(
                                        "{} ({})",
                                        self.tr("Authorized devices"),
                                        self.authorized.len()
                                    )),
                            )
                            .child(self.action_button(
                                "add-certificate",
                                self.tr("Add"),
                                ACCENT,
                                0x081018,
                                cx,
                                |this, _, window, _| {
                                    this.open_certificate_dialog();
                                    window.focus(&this.add_focus);
                                },
                            )),
                    )
                    .when(self.authorized.is_empty(), |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(rgb(MUTED))
                                .child(self.tr("No authorized devices.")),
                        )
                    })
                    .children(
                        self.authorized
                            .iter()
                            .map(|(fingerprint, name)| (fingerprint.clone(), name.clone()))
                            .collect::<std::collections::BTreeMap<_, _>>()
                            .into_iter()
                            .map(|(fingerprint, name)| {
                                let remove_fp = fingerprint.clone();
                                div()
                                    .id(SharedString::from(format!("cert-{fingerprint}")))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_1()
                                            .child(
                                                div().text_sm().text_color(rgb(TEXT)).child(name),
                                            )
                                            .child(div().text_xs().text_color(rgb(MUTED)).child(
                                                short_fingerprint(&fingerprint, 24),
                                            )),
                                    )
                                    .child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "remove-cert-{fingerprint}"
                                            )))
                                            .px_3()
                                            .py_2()
                                            .rounded_md()
                                            .bg(rgb(0x2a2020))
                                            .text_color(rgb(RED))
                                            .text_sm()
                                            .cursor_pointer()
                                            .child(self.tr("Remove"))
                                            .on_click(cx.listener(move |this, _, _, _| {
                                                this.send(FrontendRequest::RemoveAuthorizedKey(
                                                    remove_fp.clone(),
                                                ))
                                            })),
                                    )
                            }),
                    ),
            )
            .when(!self.pairing_review.is_empty(), |this| {
                this.child(self.render_pairing_review(cx))
            })
            .child(self.render_device_permissions(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(CARD))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(TEXT))
                                    .child(format!(
                                        "{} ({})",
                                        self.tr("Allowed addresses"),
                                        self.address_rules.len()
                                    )),
                            )
                            .child(self.action_button(
                                "add-address-rule",
                                self.tr("Add"),
                                ACCENT,
                                0x081018,
                                cx,
                                |this, _, window, _| {
                                    this.open_address_dialog();
                                    window.focus(&this.add_focus);
                                },
                            )),
                    )
                    .when(self.address_rules.is_empty(), |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(rgb(MUTED))
                                .child(self.tr("No address rules.")),
                        )
                    })
                    .children(self.address_rules.clone().into_iter().map(|rule| {
                        let address = rule.address.clone();
                        let broad = rule.broad;
                        div()
                            .id(SharedString::from(format!("rule-{}", rule.address)))
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(TEXT))
                                            .child(rule.address.clone()),
                                    )
                                    .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                                        "in:{} · out:{}",
                                        rule.incoming.capabilities.summary(),
                                        rule.outgoing.capabilities.summary()
                                    )))
                                    .when(broad, |this| {
                                        this.child(div().text_xs().text_color(rgb(AMBER)).child(
                                            "⚠ broad range: this can auto-authorize devices outside your LAN",
                                        ))
                                    }),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("remove-rule-{}", address)))
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .bg(rgb(0x2a2020))
                                    .text_color(rgb(RED))
                                    .text_sm()
                                    .cursor_pointer()
                                    .child(self.tr("Remove"))
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        this.send(FrontendRequest::UntrustAddress(address.clone()))
                                    })),
                            )
                    })),
            )
            .child(self.render_settings_advanced(cx))
            .child(self.render_settings_about())
    }
}

fn main_window_options(cx: &mut App) -> WindowOptions {
    let bounds = Bounds::centered(None, size(px(980.0), px(720.0)), cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(880.0), px(600.0))),
        titlebar: Some(TitlebarOptions {
            title: Some("Lan Mouse".into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        app_id: Some("de.feschber.LanMouse".into()),
        window_decorations: if cfg!(target_os = "linux") {
            Some(WindowDecorations::Client)
        } else {
            None
        },
        ..Default::default()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GpuiError {
    #[error("GPUI application exited unexpectedly")]
    ApplicationExited,
}

/// Why the GPUI frontend process is exiting.
///
/// The GPUI platform loop returns in both cases; the caller must distinguish
/// them because a closed window must not stop the background service (and the
/// LAN connection), while an explicit quit must.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrontendExit {
    /// The user asked to quit (tray or Settings): the service was asked to shut
    /// down over IPC and the launcher may stop the daemon it started.
    Quit,
    /// The last window was closed: keep the service and the LAN connection
    /// running. Launching Lan Mouse again reconnects a new window to it.
    WindowClosed,
}

/// Run the GPUI frontend. The caller (the `lan-mouse` binary) starts the
/// service/IPC first, exactly as it does for the GTK frontend.
pub fn run(local_commit: [u8; 8]) -> Result<FrontendExit, GpuiError> {
    let startup = std::time::Instant::now();
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .try_init();
    log::info!(
        "starting GPUI frontend (build {})",
        String::from_utf8_lossy(&local_commit)
    );

    Application::new().run(move |cx: &mut App| {
        set_ui_scale(automatic_ui_scale(cx));
        log::info!("UI scale: {:.0}%", ui_scale() * 100.0);
        let dashboard: Entity<Dashboard> = cx.new(|cx| Dashboard::new(cx, local_commit));
        let reopen_dashboard = dashboard.clone();
        let options = main_window_options(cx);
        let _window_handle = match cx.open_window(options, |window, _| {
            window.set_rem_size(px(16.0));
            dashboard
        }) {
            Ok(window_handle) => window_handle,
            Err(error) => {
                // No compositor / GPU initialization failure: report it instead
                // of panicking (release builds abort on panic). Treat it as an
                // explicit quit so the launcher does not leave a service behind
                // for a UI that never appeared.
                log::error!("could not open the Lan Mouse window: {error:?}");
                request_quit();
                cx.quit();
                return;
            }
        };
        log::info!("GPUI window created in {:?}", startup.elapsed());

        let tray_rx = tray::spawn();
        cx.spawn(async move |cx| {
            while let Ok(command) = tray_rx.recv().await {
                match command {
                    tray::Command::Open => {
                        let dashboard = reopen_dashboard.clone();
                        let result = cx.update(|cx| {
                            let mut windows = cx.windows();
                            if let Some(handle) = windows.pop() {
                                return handle.update(cx, |_, window, _| {
                                    window.activate_window();
                                });
                            }
                            // X closed the window; recreate it while the
                            // process (and the LAN connection) keeps running.
                            let options = main_window_options(cx);
                            let opened = cx.open_window(options, |window, _| {
                                window.set_rem_size(px(16.0));
                                dashboard
                            });
                            match opened {
                                Ok(_) => {
                                    cx.activate(true);
                                    Ok(())
                                }
                                Err(error) => {
                                    log::error!("could not reopen the Lan Mouse window: {error:?}");
                                    Err(error)
                                }
                            }
                        });
                        let _ = result;
                    }
                    tray::Command::Quit => {
                        // Explicit quit: stop the service over IPC (it may not
                        // be owned by the launcher when it was started by
                        // systemd or a previous launch), then let the GPUI loop
                        // return so the process exits gracefully.
                        let dashboard = reopen_dashboard.clone();
                        let _ = cx.update(|cx| {
                            dashboard.update(cx, |this, _| {
                                this.send(FrontendRequest::Shutdown);
                            });
                        });
                        request_quit();
                        let _ = cx.update(|cx| cx.quit());
                        break;
                    }
                }
            }
        })
        .detach();

        cx.activate(true);
    });

    // `Application::run` returns when the platform loop stops: either an
    // explicit quit (tray/Settings/startup failure) or the last window being
    // destroyed. The window close must keep the service alive, so the two
    // cases are distinguished by the quit flag set by those explicit paths.
    if QUIT_REQUESTED.load(Ordering::SeqCst) {
        log::info!("GPUI frontend quit requested; the launcher stops the service");
        Ok(FrontendExit::Quit)
    } else {
        log::info!("GPUI window closed; keeping the service and the LAN connection running");
        Ok(FrontendExit::WindowClosed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    fn config(hostname: Option<&str>, fingerprint: Option<&str>) -> ClientConfig {
        ClientConfig {
            hostname: hostname.map(str::to_string),
            fingerprint: fingerprint.map(str::to_string),
            port: lan_mouse_ipc::DEFAULT_PORT,
            ..Default::default()
        }
    }

    fn state(
        active: bool,
        alive: bool,
        active_addr: Option<&str>,
        incoming_addr: Option<&str>,
        last_pong_at: Option<u64>,
    ) -> ClientState {
        ClientState {
            active,
            alive,
            active_addr: active_addr.map(|addr| addr.parse().expect("address")),
            incoming_addr: incoming_addr.map(|addr| addr.parse().expect("address")),
            last_pong_at,
            ..Default::default()
        }
    }

    fn member(
        handle: ClientHandle,
        config: ClientConfig,
        state: ClientState,
    ) -> (ClientHandle, ClientConfig, ClientState) {
        (handle, config, state)
    }

    #[test]
    fn side_selector_offers_every_side_exactly_once() {
        assert_eq!(SIDES.len(), 4);
        for side in [
            Position::Left,
            Position::Right,
            Position::Top,
            Position::Bottom,
        ] {
            assert_eq!(
                SIDES.iter().filter(|candidate| **candidate == side).count(),
                1,
                "side {side:?} must be offered exactly once"
            );
        }
    }

    #[test]
    fn position_labels_cover_all_sides() {
        assert_eq!(position_label(Position::Left), "left");
        assert_eq!(position_label(Position::Right), "right");
        assert_eq!(position_label(Position::Top), "top");
        assert_eq!(position_label(Position::Bottom), "bottom");
    }

    #[test]
    fn disabled_peer_is_offline_and_offers_connect() {
        let derived = link_state(false, false, false, false, None, NOW);
        assert_eq!(derived, LinkState::Disabled);
        assert_eq!(derived.badge(), "Disconnected");
        let action = row_action(derived, false);
        assert_eq!(action, RowAction::Connect);
        assert!(action.wants_activation());
    }

    /// Regression: an enabled client with no outgoing link yet (first handshake
    /// attempt, or the automatic retry after a drop) used to render as
    /// "Disconnected" with a "Connect" button that actually *deactivated* it.
    #[test]
    fn enabled_client_without_link_is_connecting_and_offers_cancel() {
        for alive in [false, true] {
            let derived = link_state(true, alive, false, false, None, NOW);
            assert_eq!(derived, LinkState::Connecting);
            assert_eq!(derived.badge(), "Connecting…");
            let action = row_action(derived, true);
            assert_eq!(action, RowAction::Cancel);
            assert!(
                !action.wants_activation(),
                "Cancel must not activate (it would disconnect an enabled client)"
            );
        }
    }

    /// A completed DTLS handshake is not proof that the peer answers: without a
    /// Pong the card must not claim "Connected".
    #[test]
    fn handshake_without_round_trip_is_not_connected() {
        for alive in [false, true] {
            for incoming in [false, true] {
                let derived = link_state(true, alive, true, incoming, None, NOW);
                assert_eq!(derived, LinkState::Connecting);
                assert_ne!(derived.badge(), "Connected");
            }
        }
    }

    #[test]
    fn fresh_round_trip_is_the_only_way_to_claim_connected() {
        let fresh = Some(NOW - 1_000);
        assert_eq!(
            link_state(true, true, true, false, fresh, NOW),
            LinkState::Connected
        );
        assert_eq!(
            link_state(true, true, true, true, fresh, NOW),
            LinkState::ConnectedBidirectional
        );
        assert_eq!(LinkState::Connected.badge(), "Connected");
        // Boundary: exactly the freshness window still counts as fresh.
        let boundary = Some(NOW - LINK_FRESH_WINDOW_MS);
        assert_eq!(
            link_state(true, true, true, false, boundary, NOW),
            LinkState::Connected
        );
        // A single millisecond past it is a dead link, not a healthy one.
        let stale = Some(NOW - LINK_FRESH_WINDOW_MS - 1);
        assert_eq!(
            link_state(true, true, true, false, stale, NOW),
            LinkState::Reconnecting
        );
    }

    #[test]
    fn stale_link_is_reconnecting_not_connected() {
        let stale = Some(NOW - 30_000);
        let derived = link_state(true, true, true, false, stale, NOW);
        assert_eq!(derived, LinkState::Reconnecting);
        assert_ne!(derived.badge(), "Connected");
        assert!(derived.is_degraded());
        assert_eq!(row_action(derived, true), RowAction::Disconnect);
    }

    #[test]
    fn peer_emulation_disabled_is_degraded_not_green() {
        let derived = link_state(true, false, true, false, Some(NOW), NOW);
        assert_eq!(derived, LinkState::ConnectedDegraded);
        assert!(derived.is_degraded());
        // The link is real, so the badge still says Connected — the note explains
        // the degradation.
        assert_eq!(derived.badge(), "Connected");
        assert_eq!(row_action(derived, true), RowAction::Disconnect);
    }

    #[test]
    fn incoming_session_never_claims_connected() {
        for outgoing in [false, true] {
            for pong in [None, Some(NOW), Some(NOW - 60_000)] {
                let derived = link_state(false, true, outgoing, true, pong, NOW);
                assert_eq!(derived, LinkState::Receiving);
                assert_eq!(derived.badge(), "Peer connected");
                assert_ne!(derived.badge(), "Connected");
            }
        }
        // A receiving-only peer must be disconnectable: the primary action
        // closes the incoming session (the reverse "Also connect" stays as a
        // secondary button in the card).
        assert_eq!(
            row_action(LinkState::Receiving, false),
            RowAction::Disconnect
        );
        assert!(!row_action(LinkState::Receiving, false).wants_activation());
        assert_eq!(row_action(LinkState::Receiving, true), RowAction::Cancel);
        assert!(!row_action(LinkState::Receiving, true).wants_activation());
    }

    #[test]
    fn action_table_matches_each_state() {
        let expectation = [
            (LinkState::Disabled, RowAction::Connect, true),
            (LinkState::Connecting, RowAction::Cancel, false),
            (LinkState::Reconnecting, RowAction::Disconnect, false),
            (LinkState::ConnectedDegraded, RowAction::Disconnect, false),
            (LinkState::Connected, RowAction::Disconnect, false),
            (
                LinkState::ConnectedBidirectional,
                RowAction::Disconnect,
                false,
            ),
            (LinkState::Receiving, RowAction::Cancel, false),
        ];
        for (state, expected_action, expected_activation) in expectation {
            let action = row_action(state, true);
            assert_eq!(action, expected_action, "{state:?}");
            assert_eq!(action.wants_activation(), expected_activation, "{state:?}");
            assert!(!action.label().trim().is_empty());
        }
    }

    /// Regression: one service entry per address family used to render as two
    /// device cards for the same peer (the GTK frontend fixed this by grouping
    /// on the certificate fingerprint).
    #[test]
    fn duplicate_entries_of_one_peer_are_aggregated() {
        let members = [
            member(
                1,
                config(Some("predator"), Some("AA:BB")),
                state(
                    true,
                    true,
                    Some("192.168.1.10:4242"),
                    None,
                    Some(NOW - 1_000),
                ),
            ),
            member(
                2,
                config(Some("predator"), Some("aa:bb")),
                state(
                    false,
                    false,
                    Some("[fd00::1]:4242"),
                    Some("[fd00::1]:50000"),
                    None,
                ),
            ),
        ];
        let peer = aggregate_peer(&members).expect("aggregate");
        assert_eq!(peer.primary, 1);
        assert_eq!(peer.members, vec![1, 2]);
        assert_eq!(peer.name, "predator");
        // Link data comes from the member with a proven round-trip, so the card
        // cannot flap between the two links.
        assert_eq!(peer.endpoint, "192.168.1.10:4242");
        assert!(peer.active);
        assert!(peer.has_outgoing);
        assert!(peer.has_incoming);
        assert_eq!(peer.last_pong_at, Some(NOW - 1_000));
        let derived = link_state(
            peer.active,
            peer.alive,
            peer.has_outgoing,
            peer.has_incoming,
            peer.last_pong_at,
            NOW,
        );
        assert_eq!(derived, LinkState::ConnectedBidirectional);
    }

    #[test]
    fn aggregation_prefers_the_member_with_a_round_trip() {
        let members = [
            member(
                10,
                config(None, Some("aa:bb")),
                state(true, true, Some("192.168.1.10:4242"), None, None),
            ),
            member(
                11,
                config(None, Some("aa:bb")),
                state(true, false, Some("[fd00::1]:4242"), None, Some(NOW - 500)),
            ),
        ];
        let peer = aggregate_peer(&members).expect("aggregate");
        assert_eq!(peer.endpoint, "[fd00::1]:4242");
        assert!(!peer.alive);
        // No hostname: falls back to an unknown-device label rather than an
        // empty card title.
        assert_eq!(peer.name, "Unknown device");
    }

    #[test]
    fn peers_are_grouped_by_fingerprint() {
        let mut clients = HashMap::new();
        clients.insert(1, (config(None, Some("AA:BB")), ClientState::default()));
        clients.insert(2, (config(None, Some("aa:bb")), ClientState::default()));
        clients.insert(3, (config(Some("loner"), None), ClientState::default()));
        clients.insert(4, (config(None, Some("aa:bb")), ClientState::default()));
        let groups = group_peer_handles(&clients);
        assert_eq!(groups, vec![vec![1, 2, 4], vec![3]]);
        // An empty map has no groups and therefore no cards.
        assert!(group_peer_handles(&HashMap::new()).is_empty());
    }

    #[test]
    fn multibyte_fingerprint_truncation_does_not_panic() {
        // Fingerprints are normalized hex today, but a config file can carry
        // arbitrary text: byte slicing used to be able to panic here.
        let weird = "aé𝒶🧪 fingerprint";
        let short = short_fingerprint(weird, 3);
        assert_eq!(short.chars().count(), 4, "3 chars + ellipsis");
        assert!(short.ends_with('…'));
        // Short strings are not decorated.
        assert_eq!(short_fingerprint("ab", 8), "ab");
    }

    #[test]
    fn fresh_pong_requires_a_known_and_pinned_fingerprint() {
        let mut clients = HashMap::new();
        clients.insert(
            7,
            (
                config(None, Some("AA:BB")),
                state(true, true, None, None, Some(NOW - 1_000)),
            ),
        );
        assert!(has_fresh_pong(&clients, "aa:bb", NOW));
        assert!(has_fresh_pong(&clients, " AA:BB ", NOW));
        assert!(!has_fresh_pong(&clients, "cc:dd", NOW));
        assert!(!has_fresh_pong(&clients, "", NOW));
        // Stale proof must not count.
        assert!(!has_fresh_pong(
            &clients,
            "aa:bb",
            NOW + LINK_FRESH_WINDOW_MS + 1
        ));
        // A client without a fingerprint cannot be matched to a device identity.
        let mut unpinned = HashMap::new();
        unpinned.insert(
            8,
            (config(None, None), state(true, true, None, None, Some(NOW))),
        );
        assert!(!has_fresh_pong(&unpinned, "aa:bb", NOW));
    }

    #[test]
    fn endpoint_debounce_does_not_flap_between_families() {
        let hold = std::time::Duration::from_secs(3);
        let start = std::time::Instant::now();
        let mut shown = None;
        let mut candidate = None;
        let ipv4 = Some("192.168.1.10:4242".to_string());
        let ipv6 = Some("[fd00::1]:4242".to_string());
        assert_eq!(
            sticky_endpoint(&mut shown, &mut candidate, ipv4.clone(), start, hold),
            ipv4
        );
        // A different endpoint from the next packet does not replace it yet.
        for offset in [0u64, 1, 2] {
            assert_eq!(
                sticky_endpoint(
                    &mut shown,
                    &mut candidate,
                    ipv6.clone(),
                    start + std::time::Duration::from_secs(offset),
                    hold
                ),
                ipv4
            );
        }
        // Once it persisted for the hold window, the new one is shown.
        assert_eq!(
            sticky_endpoint(
                &mut shown,
                &mut candidate,
                ipv6.clone(),
                start + std::time::Duration::from_secs(3),
                hold
            ),
            ipv6
        );
        // Losing the link clears the shown endpoint.
        assert_eq!(
            sticky_endpoint(&mut shown, &mut candidate, None, start, hold),
            None
        );
    }

    #[test]
    fn timestamp_age_is_human_readable() {
        let now_ms = NOW;
        let now_secs = now_ms / 1000;
        assert_eq!(format_timestamp_age(now_secs - 5, now_ms), "5s ago");
        assert_eq!(format_timestamp_age(now_secs - 90, now_ms), "1m ago");
        assert_eq!(format_timestamp_age(now_secs - 7_200, now_ms), "2h ago");
        assert_eq!(format_timestamp_age(now_secs - 90_000, now_ms), "1d ago");
        // Clock skew (timestamp in the future) must not underflow.
        assert_eq!(format_timestamp_age(now_secs + 60, now_ms), "0s ago");
    }

    #[test]
    fn pending_add_is_not_consumed_by_an_unrelated_created_event() {
        let mut pending = Some(PendingAdd::Manual {
            hostname: "DESKTOP-GABRIEL".to_string(),
            port: 4242,
            fingerprint: None,
            position: Position::Left,
        });
        let auto_reverse = ClientConfig {
            hostname: Some("DESKTOP-GABRIEL".to_string()),
            fix_ips: vec!["192.168.1.42".parse().expect("ip")],
            ..ClientConfig::default()
        };
        assert!(
            take_pending_add(&mut pending, &auto_reverse).is_none(),
            "an auto-reverse/config-reload Created must not steal the pending Add"
        );
        assert!(pending.is_some());

        let fresh = ClientConfig::default();
        assert!(take_pending_add(&mut pending, &fresh).is_some());
        assert!(
            pending.is_none(),
            "the empty Create response consumes it exactly once"
        );
    }

    #[test]
    fn fresh_client_detection_rejects_configured_entries() {
        assert!(is_fresh_client_config(&ClientConfig::default()));
        let configured_port = ClientConfig {
            port: 4243,
            ..ClientConfig::default()
        };
        assert!(!is_fresh_client_config(&configured_port));
        let configured_pin = ClientConfig {
            fingerprint: Some("aa".to_string()),
            ..ClientConfig::default()
        };
        assert!(!is_fresh_client_config(&configured_pin));
    }

    #[test]
    fn network_add_buttons_have_unique_element_ids() {
        let a: std::net::IpAddr = "192.168.1.42".parse().expect("ip");
        let b: std::net::IpAddr = "192.168.1.43".parse().expect("ip");
        assert_ne!(network_add_id(a, 4242), network_add_id(b, 4242));
        assert_ne!(network_add_id(a, 4242), network_add_id(a, 4243));
        assert_eq!(network_add_id(a, 4242), network_add_id(a, 4242));
    }

    #[test]
    fn outgoing_sensitivity_is_only_offered_by_the_controlling_side() {
        assert!(outgoing_sensitivity_visible(true, false));
        assert!(
            !outgoing_sensitivity_visible(true, true),
            "a receiving-only client must not show the outgoing sensitivity control"
        );
        assert!(!outgoing_sensitivity_visible(false, false));
    }

    #[test]
    fn local_ipv4_discovery_returns_only_non_loopback_ipv4() {
        let ips = discover_local_ipv4();
        assert!(ips.len() <= 1, "only the primary IPv4 should be shown");
        for ip in ips {
            let address: std::net::IpAddr = ip.parse().expect("valid IP string");
            let std::net::IpAddr::V4(v4) = address else {
                panic!("discovery returned a non-IPv4 address: {address}");
            };
            assert!(!v4.is_loopback());
            assert!(!v4.is_link_local());
        }
    }

    #[test]
    fn peer_groups_group_by_fingerprint_and_keep_unknowns_separate() {
        let mut clients = std::collections::HashMap::new();
        clients.insert(
            2,
            (
                config(Some("b"), Some("aa:bb")),
                state(false, false, None, None, None),
            ),
        );
        clients.insert(
            0,
            (
                config(Some("a"), Some("aa:bb")),
                state(false, false, None, None, None),
            ),
        );
        clients.insert(
            1,
            (
                config(Some("unknown"), None),
                state(false, false, None, None, None),
            ),
        );
        clients.insert(
            3,
            (
                config(Some("unknown2"), None),
                state(false, false, None, None, None),
            ),
        );

        assert_eq!(
            group_peer_handles(&clients),
            vec![vec![0, 2], vec![1], vec![3]],
            "same certificate groups together; unknown identities stay separate and sorted"
        );
    }

    #[test]
    fn sticky_endpoint_holds_a_flapping_endpoint_until_the_window_expires() {
        use std::time::{Duration, Instant};
        let hold = Duration::from_secs(3);
        let now = Instant::now();
        let mut shown = None;
        let mut candidate = None;

        assert_eq!(
            sticky_endpoint(&mut shown, &mut candidate, Some("a".into()), now, hold),
            Some("a".to_string())
        );
        assert_eq!(
            sticky_endpoint(
                &mut shown,
                &mut candidate,
                Some("b".into()),
                now + Duration::from_millis(1),
                hold
            ),
            Some("a".to_string()),
            "a new endpoint must not replace the shown one immediately"
        );
        assert_eq!(
            sticky_endpoint(
                &mut shown,
                &mut candidate,
                Some("b".into()),
                now + Duration::from_millis(1) + hold,
                hold
            ),
            Some("b".to_string())
        );
        assert_eq!(
            sticky_endpoint(&mut shown, &mut candidate, None, now + hold, hold),
            None
        );
    }

    #[test]
    fn aggregate_peer_uses_the_member_with_a_live_link() {
        let members = [
            member(
                1,
                config(Some("peer"), Some("aa:bb")),
                state(false, false, None, Some("127.0.0.1:5000"), None),
            ),
            member(
                2,
                config(Some("peer"), Some("aa:bb")),
                state(true, true, Some("127.0.0.1:4242"), None, Some(NOW)),
            ),
        ];
        let aggregate = aggregate_peer(&members).expect("aggregate");
        assert_eq!(aggregate.members, vec![1, 2]);
        assert!(aggregate.active);
        assert!(aggregate.has_outgoing);
        assert!(aggregate.last_pong_at.is_some());
        assert_eq!(aggregate.position, lan_mouse_ipc::Position::Left);
    }
}
