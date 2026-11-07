use std::collections::HashMap;

use gpui::{
    App, Application, Bounds, Context, Entity, FocusHandle, KeyDownEvent, Render, SharedString,
    Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};
use lan_mouse_ipc::{
    AddressRuleView, ClientConfig, ClientHandle, ClientState, ClipboardStatus, DeviceView,
    DirectionPolicy, DiscoveredDevice, FrontendEvent, FrontendRequest, FrontendRequestWriter,
    PendingView, Position, Status,
};

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
        std::thread::Builder::new()
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
            })
            .expect("spawn tray thread");
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
        // 16x16 solid accent icon; the production frontend will use the
        // embedded Lan Mouse icon resource.
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

#[cfg(not(feature = "tray"))]
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

#[derive(Clone, Debug)]
struct ClientRow {
    handle: ClientHandle,
    name: String,
    endpoint: String,
    state: String,
    action: &'static str,
    active: bool,
    healthy: bool,
}

impl ClientRow {
    fn from_state(config: &ClientConfig, state: &ClientState) -> Self {
        let name = config
            .hostname
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| {
                config
                    .fix_ips
                    .iter()
                    .next()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "Unknown device".to_string())
            });
        let endpoint = state
            .active_addr
            .or(state.incoming_addr)
            .map(|addr| addr.to_string())
            .or_else(|| config.fix_ips.iter().next().map(ToString::to_string))
            .unwrap_or_else(|| format!("port {}", config.port));
        let healthy = state.last_pong_at.is_some_and(|pong| {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            now_ms.saturating_sub(pong) < 6_000
        });
        let (state_label, action) = if !state.active && state.incoming_addr.is_none() {
            ("Disconnected", "Connect")
        } else if state.active && healthy {
            ("Connected", "Disconnect")
        } else if state.active && state.active_addr.is_some() {
            ("Connecting…", "Cancel")
        } else if state.incoming_addr.is_some() {
            ("Peer connected", "Also connect")
        } else {
            ("Disconnected", "Connect")
        };
        Self {
            handle: 0,
            name,
            endpoint,
            state: state_label.to_string(),
            action,
            active: state.active,
            healthy,
        }
    }
}

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
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut ips = interfaces
        .into_iter()
        .filter_map(|interface| match interface.addr {
            if_addrs::IfAddr::V4(v4) if !v4.ip.is_loopback() && !v4.ip.is_link_local() => {
                Some(v4.ip.to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    ips.sort();
    ips.dedup();
    ips
}

#[derive(Clone, Debug)]
enum PendingAdd {
    Discovered {
        ip: std::net::IpAddr,
        port: u16,
    },
    Manual {
        hostname: String,
        port: u16,
        fingerprint: Option<String>,
    },
}

#[derive(Clone, Debug)]
struct AddDeviceDialog {
    hostname: String,
    port: String,
    fingerprint: String,
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
    Client { handle: ClientHandle },
    RenameDevice { fingerprint: String },
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

    fn new_client(handle: ClientHandle, config: &ClientConfig) -> Self {
        Self {
            kind: SettingsDialogKind::Client { handle },
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

    fn field_count(&self) -> u8 {
        match self.kind {
            SettingsDialogKind::Address => 1,
            SettingsDialogKind::Certificate | SettingsDialogKind::Client { .. } => 2,
            SettingsDialogKind::RenameDevice { .. } => 1,
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
        }
    }
}

impl AddDeviceDialog {
    fn new() -> Self {
        Self {
            hostname: String::new(),
            port: lan_mouse_ipc::DEFAULT_PORT.to_string(),
            fingerprint: String::new(),
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

enum BridgeMessage {
    Writer(FrontendRequestWriter),
    Event(Box<FrontendEvent>),
    Error(String),
}

struct Dashboard {
    writer: Option<FrontendRequestWriter>,
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
    clipboard_status: ClipboardStatus,
    pairing_active: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Devices,
    Network,
    Settings,
}

impl Dashboard {
    fn new(cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::unbounded();

        std::thread::spawn(move || {
            let (mut reader, writer) = match lan_mouse_ipc::connect() {
                Ok(connection) => connection,
                Err(error) => {
                    let _ = tx.send_blocking(BridgeMessage::Error(error.to_string()));
                    return;
                }
            };
            let _ = tx.send_blocking(BridgeMessage::Writer(writer));
            while let Some(event) = reader.next_event() {
                match event {
                    Ok(event) => {
                        if tx
                            .send_blocking(BridgeMessage::Event(Box::new(event)))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => log::warn!("frontend event error: {error}"),
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

        Self {
            writer: None,
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
            status: "Connecting to Lan Mouse service…".to_string(),
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
            clipboard_status: ClipboardStatus::default(),
            pairing_active: false,
        }
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
                let _ = writer.request(FrontendRequest::Sync);
                self.writer = Some(writer);
                self.status = "Connected to service".to_string();
                log::info!("GPUI frontend connected to the Lan Mouse service");
            }
            BridgeMessage::Event(event) => self.handle_event(*event),
            BridgeMessage::Error(error) => {
                self.status = format!("Service error: {error}");
            }
        }
    }

    fn handle_event(&mut self, event: FrontendEvent) {
        match event {
            FrontendEvent::Enumerate(rows) => {
                self.clients.clear();
                for (handle, config, state) in rows {
                    self.clients.insert(handle, (config, state));
                }
                log::info!("GPUI frontend received {} client(s)", self.clients.len());
            }
            FrontendEvent::Created(handle, config, state) => {
                self.clients.insert(handle, (config, state));
                if let Some(pending) = self.pending_add.take() {
                    match pending {
                        PendingAdd::Discovered { ip, port } => {
                            self.send(FrontendRequest::UpdateFixIps(handle, vec![ip]));
                            self.send(FrontendRequest::UpdatePort(handle, port));
                        }
                        PendingAdd::Manual {
                            hostname,
                            port,
                            fingerprint,
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
                        }
                    }
                    self.send(FrontendRequest::UpdatePosition(handle, Position::Left));
                    self.send(FrontendRequest::Activate(handle, true));
                }
            }
            FrontendEvent::State(handle, config, state) => {
                self.clients.insert(handle, (config, state));
            }
            FrontendEvent::Deleted(handle) => {
                self.clients.remove(&handle);
                if self.selected_client == Some(handle) {
                    self.selected_client = None;
                }
            }
            FrontendEvent::PendingRequests(pending) => self.pending = pending,
            FrontendEvent::Hello { ipc_version } => {
                self.status = format!("Service connected (IPC v{ipc_version})");
            }
            FrontendEvent::Error(error) => self.status = error,
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
            FrontendEvent::CaptureStatus(status) => self.capture_status = status,
            FrontendEvent::EmulationStatus(status) => self.emulation_status = status,
            FrontendEvent::ClipboardStatus(status) => self.clipboard_status = status,
            FrontendEvent::PairingActive(active) => self.pairing_active = active,
            FrontendEvent::Diagnostics(_) => {}
            _ => {}
        }
    }

    fn send(&mut self, request: FrontendRequest) {
        if let Some(writer) = self.writer.as_mut() {
            if let Err(error) = writer.request(request) {
                self.status = format!("IPC write error: {error}");
            }
        }
    }

    fn toggle_client(&mut self, handle: ClientHandle) {
        let active = self
            .clients
            .get(&handle)
            .map(|(_, state)| state.active)
            .unwrap_or(false);
        self.send(FrontendRequest::Activate(handle, !active));
        self.selected_client = Some(handle);
    }

    fn switch_page(&mut self, page: Page) {
        self.page = page;
        if page == Page::Network && self.discovered.is_empty() {
            self.send(FrontendRequest::ScanNetwork);
        }
    }

    fn open_address_dialog(&mut self) {
        self.settings_dialog = Some(SettingsDialog::new_address());
    }

    fn open_certificate_dialog(&mut self) {
        self.settings_dialog = Some(SettingsDialog::new_certificate());
    }

    fn open_client_edit(&mut self, handle: ClientHandle) {
        if let Some((config, _)) = self.clients.get(&handle) {
            self.settings_dialog = Some(SettingsDialog::new_client(handle, config));
        }
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
            SettingsDialogKind::RenameDevice { fingerprint } => {
                let name = dialog.name.trim();
                if !name.is_empty() {
                    self.send(FrontendRequest::RenameDevice {
                        fingerprint,
                        name: name.to_string(),
                    });
                }
            }
            SettingsDialogKind::Client { handle } => {
                let hostname = dialog.hostname.trim().to_string();
                self.send(FrontendRequest::UpdateHostname(
                    handle,
                    (!hostname.is_empty()).then_some(hostname),
                ));
                if let Ok(port) = dialog.port.trim().parse::<u16>() {
                    self.send(FrontendRequest::UpdatePort(handle, port));
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
        let Some(dialog) = self.add_dialog.take() else {
            return;
        };
        let port = dialog
            .port
            .trim()
            .parse::<u16>()
            .unwrap_or(lan_mouse_ipc::DEFAULT_PORT);
        let fingerprint = dialog
            .fingerprint
            .trim()
            .is_empty()
            .then_some(None)
            .unwrap_or_else(|| Some(dialog.fingerprint.trim().to_string()));
        self.pending_add = Some(PendingAdd::Manual {
            hostname: dialog.hostname,
            port,
            fingerprint,
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

    fn add_discovered(&mut self, ip: std::net::IpAddr, port: u16) {
        self.pending_add = Some(PendingAdd::Discovered { ip, port });
        self.send(FrontendRequest::Create);
    }

    fn cycle_position(&mut self, handle: ClientHandle, current: Position) {
        let next = match current {
            Position::Left => Position::Right,
            Position::Right => Position::Top,
            Position::Top => Position::Bottom,
            Position::Bottom => Position::Left,
        };
        self.send(FrontendRequest::UpdatePosition(handle, next));
    }

    fn adjust_sensitivity(&mut self, handle: ClientHandle, current: u16, delta: i32) {
        let next = (i32::from(current) + delta).clamp(10, 500) as u16;
        self.send(FrontendRequest::UpdateSensitivity(handle, next));
    }

    fn resolve_dns(&mut self, handle: ClientHandle) {
        self.send(FrontendRequest::ResolveDns(handle));
    }

    fn delete_client(&mut self, handle: ClientHandle) {
        self.send(FrontendRequest::Delete(handle));
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

    fn active_count(&self) -> usize {
        self.clients
            .values()
            .filter(|(_, state)| state.active)
            .count()
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

    fn render_client(
        &self,
        handle: ClientHandle,
        (config, state): &(ClientConfig, ClientState),
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let mut row = ClientRow::from_state(config, state);
        row.handle = handle;
        let position = config.pos;
        let sensitivity = config.sensitivity;
        let (state_color, state_bg) = if row.healthy {
            (GREEN, 0x173225)
        } else if row.active {
            (AMBER, 0x342b12)
        } else {
            (MUTED, 0x1d222a)
        };
        div()
            .flex()
            .flex_col()
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
                                    .child(row.name.clone()),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(MUTED))
                                    .child(row.endpoint.clone()),
                            ),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_full()
                            .bg(rgb(state_bg))
                            .text_color(rgb(state_color))
                            .text_xs()
                            .child(row.state.clone()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id(SharedString::from(format!("toggle-{}", row.handle)))
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(rgb(if row.active { 0x2a2020 } else { 0x1b2a3a }))
                            .text_color(rgb(if row.active { RED } else { ACCENT }))
                            .text_sm()
                            .cursor_pointer()
                            .child(row.action)
                            .on_click(cx.listener(move |this, _, _, _| this.toggle_client(handle))),
                    )
                    .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                        "{}",
                        if row.healthy {
                            "link healthy"
                        } else if row.active {
                            "waiting for reply"
                        } else {
                            "offline"
                        }
                    ))),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                        "position {} · sensitivity {}%",
                        position_label(position),
                        sensitivity
                    )))
                    .child(self.action_button(
                        "edit-client",
                        "Edit",
                        0x1d2430,
                        TEXT,
                        cx,
                        move |this, _, window, _| {
                            this.open_client_edit(handle);
                            window.focus(&this.add_focus);
                        },
                    ))
                    .child(self.action_button(
                        "resolve-dns",
                        "DNS",
                        0x1d222a,
                        TEXT,
                        cx,
                        move |this, _, _, _| this.resolve_dns(handle),
                    ))
                    .child(self.action_button(
                        "position",
                        "Position",
                        0x1d222a,
                        TEXT,
                        cx,
                        move |this, _, _, _| this.cycle_position(handle, position),
                    ))
                    .child(self.action_button(
                        "sensitivity-down",
                        "-",
                        0x1d222a,
                        TEXT,
                        cx,
                        move |this, _, _, _| this.adjust_sensitivity(handle, sensitivity, -10),
                    ))
                    .child(self.action_button(
                        "sensitivity-up",
                        "+",
                        0x1d222a,
                        TEXT,
                        cx,
                        move |this, _, _, _| this.adjust_sensitivity(handle, sensitivity, 10),
                    ))
                    .child(self.action_button(
                        "delete-client",
                        "Remove",
                        0x3a2020,
                        RED,
                        cx,
                        move |this, _, _, _| this.delete_client(handle),
                    )),
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
            "configured".to_string()
        } else if device.lan_mouse {
            "Lan Mouse available".to_string()
        } else {
            "host reachable (Lan Mouse not confirmed)".to_string()
        };
        let version = device
            .version
            .clone()
            .map(|version| format!(" · build {version}"))
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
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("{}:{} · {status}{version}", device.ip, port)),
                    ),
            )
            .when(!device.configured, |this| {
                this.child(self.action_button(
                    "add-discovered",
                    "Add",
                    ACCENT,
                    0x081018,
                    cx,
                    move |this, _, _, _| this.add_discovered(device.ip, port),
                ))
            })
    }
}

impl Render for Dashboard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar = div()
            .w(px(220.0))
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
                    .child(self.sidebar_item(Page::Devices, "Devices", "◉", cx))
                    .child(self.sidebar_item(Page::Network, "Network", "⌁", cx))
                    .child(self.sidebar_item(Page::Settings, "Settings", "⚙", cx)),
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
                            .child(format!("{} active link(s)", self.active_count())),
                    ),
            );

        let content = match self.page {
            Page::Devices => self.render_devices(cx).into_any_element(),
            Page::Network => self.render_network(cx).into_any_element(),
            Page::Settings => self.render_settings(cx).into_any_element(),
        };

        div().flex().size_full().bg(rgb(BG)).child(sidebar).child(
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
                                    Page::Devices => "Devices",
                                    Page::Network => "Network",
                                    Page::Settings => "Settings",
                                }),
                        )
                        .child(
                            div()
                                .id("add-device")
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .bg(rgb(ACCENT))
                                .text_color(rgb(0x081018))
                                .text_sm()
                                .cursor_pointer()
                                .child("+ Add device")
                                .on_click(cx.listener(|this, _, window, _| {
                                    this.add_device();
                                    window.focus(&this.add_focus);
                                })),
                        ),
                )
                .when(self.settings_dialog.is_some(), |this| {
                    this.child(self.render_settings_dialog(cx))
                })
                .child(content),
        )
    }
}

impl Dashboard {
    fn action_button(
        &self,
        id: &'static str,
        label: &'static str,
        background: u32,
        foreground: u32,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &gpui::ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(SharedString::from(id))
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
                "Add allowed address",
                "IPv4 address or CIDR. Tab switches fields, Enter saves, Esc cancels.",
                vec![("address", dialog.address.clone())],
            ),
            SettingsDialogKind::Certificate => (
                "Authorize certificate",
                "Fingerprint and display name of the peer. Tab switches fields.",
                vec![
                    ("name", dialog.name.clone()),
                    ("fingerprint", dialog.fingerprint.clone()),
                ],
            ),
            SettingsDialogKind::Client { .. } => (
                "Edit client",
                "Hostname/IP and port. Leave the hostname empty to use the address.",
                vec![
                    ("hostname / IP", dialog.hostname.clone()),
                    ("port", dialog.port.clone()),
                ],
            ),
            SettingsDialogKind::RenameDevice { .. } => (
                "Rename device",
                "Display name only; the certificate fingerprint stays the identity.",
                vec![("display name", dialog.name.clone())],
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
                        .child(label),
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
                        "Save",
                        ACCENT,
                        0x081018,
                        cx,
                        |this, _, _, _| this.submit_settings_dialog(),
                    ))
                    .child(self.action_button(
                        "settings-dialog-cancel",
                        "Cancel",
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
                    .child("Add device"),
            )
            .child(div().text_xs().text_color(rgb(MUTED)).child(
                "Type a hostname or IPv4 address. Tab switches fields, Enter adds, Esc cancels.",
            ))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(self.action_button(
                        "add-field-host",
                        "Host",
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
                        "Port",
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
                        "Fingerprint",
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
                            .child("hostname / IP"),
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
                            .child("port"),
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
                        "Cancel",
                        0x1d222a,
                        TEXT,
                        cx,
                        |this, _, _, _| this.close_add_dialog(),
                    )),
            )
            .into_any_element()
    }

    fn render_identity_card(&self) -> impl IntoElement + use<> {
        let ips = if self.local_ips.is_empty() {
            "no IPv4 address".to_string()
        } else {
            self.local_ips.join(" · ")
        };
        let fingerprint = self
            .public_fingerprint
            .as_deref()
            .map(|fp| format!("{}…", &fp[..fp.len().min(16)]))
            .unwrap_or_else(|| "fingerprint not received".to_string());
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
                            .child(format!("{} (this computer)", self.hostname)),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_full()
                            .bg(rgb(0x173225))
                            .text_color(rgb(GREEN))
                            .text_xs()
                            .child(format!("port {}", self.port)),
                    ),
            )
            .child(div().text_sm().text_color(rgb(MUTED)).child(ips))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(format!("certificate {fingerprint}")),
            )
    }

    fn render_pending(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let pending = self.pending.clone();
        let mut cards = Vec::new();
        for request in pending {
            let approve_fp = request.fingerprint.clone();
            let control_me_fp = request.fingerprint.clone();
            let i_control_fp = request.fingerprint.clone();
            let deny_fp = request.fingerprint.clone();
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
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(self.action_button(
                            "approve-pending",
                            "Approve both",
                            GREEN,
                            0x081018,
                            cx,
                            move |this, _, _, _| this.approve_pending(approve_fp.clone()),
                        ))
                        .child(self.action_button(
                            "approve-control-me",
                            "Control me",
                            0x1d2430,
                            TEXT,
                            cx,
                            move |this, _, _, _| {
                                this.respond_pending_with(
                                    control_me_fp.clone(),
                                    DirectionPolicy::allow_all(),
                                    DirectionPolicy::deny(),
                                )
                            },
                        ))
                        .child(self.action_button(
                            "approve-i-control",
                            "I control it",
                            0x1d2430,
                            TEXT,
                            cx,
                            move |this, _, _, _| {
                                this.respond_pending_with(
                                    i_control_fp.clone(),
                                    DirectionPolicy::deny(),
                                    DirectionPolicy::allow_all(),
                                )
                            },
                        ))
                        .child(self.action_button(
                            "deny-pending",
                            "Deny",
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

    fn render_devices(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut cards = Vec::new();
        let mut handles: Vec<ClientHandle> = self.clients.keys().copied().collect();
        handles.sort_unstable();
        for handle in handles {
            if let Some(entry) = self.clients.get(&handle) {
                cards.push(self.render_client(handle, entry, cx).into_any_element());
            }
        }

        let pending = self.pending.len();
        let has_pending = pending > 0;
        div()
            .id("devices-page")
            .flex()
            .flex_col()
            .gap_3()
            .p_6()
            .overflow_y_scroll()
            .child(div().flex().items_center().justify_between().child(
                div().text_sm().text_color(rgb(MUTED)).child(format!(
                    "{} configured device(s){}",
                    self.clients.len(),
                    if pending > 0 {
                        format!(" · {pending} pending request(s)")
                    } else {
                        String::new()
                    }
                )),
            ))
            .when(self.add_dialog.is_some(), |this| {
                this.child(self.render_add_dialog(cx))
            })
            .child(self.render_identity_card())
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
                                .child("No devices yet"),
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
            .flex()
            .flex_col()
            .gap_3()
            .p_6()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child("Find Lan Mouse instances on the local IPv4 network."),
                    )
                    .child(self.action_button(
                        "scan-network",
                        "Scan network",
                        ACCENT,
                        0x081018,
                        cx,
                        |this, _, _, _| this.send(FrontendRequest::ScanNetwork),
                    )),
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
                                .child("No devices discovered yet. Press Scan network."),
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

    fn render_device_permissions(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let devices = self.devices.clone();
        let mut rows = Vec::new();
        for device in devices {
            let fp = device.fingerprint.clone();
            let allow_fp = fp.clone();
            let incoming_fp = fp.clone();
            let outgoing_fp = fp.clone();
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
                                    "{}… · in:{} · out:{} · {}",
                                    &device.fingerprint[..device.fingerprint.len().min(16)],
                                    device.incoming.capabilities.summary(),
                                    device.outgoing.capabilities.summary(),
                                    device.source
                                ))),
                        )
                        .child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded_full()
                                .bg(rgb(if device.connected { 0x173225 } else { 0x1d222a }))
                                .text_color(rgb(if device.connected { GREEN } else { MUTED }))
                                .text_xs()
                                .child(if device.connected {
                                    "connected"
                                } else {
                                    "offline"
                                }),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(self.action_button(
                            "policy-both",
                            "Both",
                            0x1d2430,
                            TEXT,
                            cx,
                            move |this, _, _, _| {
                                this.set_policy(
                                    allow_fp.clone(),
                                    DirectionPolicy::allow_all(),
                                    DirectionPolicy::allow_all(),
                                )
                            },
                        ))
                        .child(self.action_button(
                            "policy-in",
                            "Control me",
                            0x1d2430,
                            TEXT,
                            cx,
                            move |this, _, _, _| {
                                this.set_policy(
                                    incoming_fp.clone(),
                                    DirectionPolicy::allow_all(),
                                    DirectionPolicy::deny(),
                                )
                            },
                        ))
                        .child(self.action_button(
                            "policy-out",
                            "I control",
                            0x1d2430,
                            TEXT,
                            cx,
                            move |this, _, _, _| {
                                this.set_policy(
                                    outgoing_fp.clone(),
                                    DirectionPolicy::deny(),
                                    DirectionPolicy::allow_all(),
                                )
                            },
                        ))
                        .child(self.action_button(
                            "policy-none",
                            "Block",
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
                            "Rename",
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
                            "Revoke",
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
                    .child(format!("Device permissions ({})", rows.len())),
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
                                .child("No known devices."),
                        ),
                )
            })
            .children(rows)
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let capture_label = if matches!(self.capture_status, Status::Enabled) {
            "capture enabled"
        } else {
            "capture disabled"
        };
        let emulation_label = if matches!(self.emulation_status, Status::Enabled) {
            "emulation enabled"
        } else {
            "emulation disabled"
        };
        let clipboard_summary = self.clipboard_status.summary();
        let pairing_label = if self.pairing_active {
            "pairing window open"
        } else {
            "open a 60 second pairing window"
        };
        let mut authorized = self
            .authorized
            .iter()
            .map(|(fingerprint, name)| {
                format!("{name} · {}…", &fingerprint[..fingerprint.len().min(16)])
            })
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
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .overflow_y_scroll()
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
                            .child("Input"),
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
                                "Reenable",
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
                                "Reenable",
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
                            .child("Clipboard"),
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
                                    "Disable"
                                } else {
                                    "Enable"
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
                            .child("Pairing"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().text_sm().text_color(rgb(MUTED)).child(pairing_label))
                            .child(self.action_button(
                                "start-pairing",
                                "Open",
                                ACCENT,
                                0x081018,
                                cx,
                                |this, _, _, _| this.start_pairing(),
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
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(TEXT))
                                    .child(format!(
                                        "Authorized devices ({})",
                                        self.authorized.len()
                                    )),
                            )
                            .child(self.action_button(
                                "add-certificate",
                                "Add",
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
                                .child("No authorized devices."),
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
                                                format!(
                                                    "{}…",
                                                    &fingerprint[..fingerprint.len().min(24)]
                                                ),
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
                                            .child("Remove")
                                            .on_click(cx.listener(move |this, _, _, _| {
                                                this.send(FrontendRequest::RemoveAuthorizedKey(
                                                    remove_fp.clone(),
                                                ))
                                            })),
                                    )
                            }),
                    ),
            )
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
                                        "Allowed addresses ({})",
                                        self.address_rules.len()
                                    )),
                            )
                            .child(self.action_button(
                                "add-address-rule",
                                "Add",
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
                                .child("No address rules."),
                        )
                    })
                    .children(self.address_rules.clone().into_iter().map(|rule| {
                        let address = rule.address.clone();
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
                                    ))),
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
                                    .child("Remove")
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        this.send(FrontendRequest::UntrustAddress(address.clone()))
                                    })),
                            )
                    })),
            )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GpuiError {
    #[error("GPUI application exited unexpectedly")]
    ApplicationExited,
}

/// Run the GPUI frontend. The caller (the `lan-mouse` binary) starts the
/// service/IPC first, exactly as it does for the GTK frontend.
pub fn run(local_commit: [u8; 8]) -> Result<(), GpuiError> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .try_init();
    log::info!(
        "starting GPUI frontend (build {})",
        String::from_utf8_lossy(&local_commit)
    );

    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(980.0), px(720.0)), cx);
        let dashboard: Entity<Dashboard> = cx.new(Dashboard::new);
        let window_handle = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, _| dashboard,
            )
            .expect("open Lan Mouse GPUI window");

        // Closing the window keeps the process alive in the tray: minimize
        // instead of destroying the window, matching the GTK close-to-tray
        // behavior until the native tray can hide/show the window directly.
        let _ = window_handle.update(cx, |_, window, cx| {
            window.on_window_should_close(cx, |window, _cx| {
                window.minimize_window();
                false
            });
        });

        let tray_rx = tray::spawn();
        cx.spawn(async move |cx| {
            while let Ok(command) = tray_rx.recv().await {
                match command {
                    tray::Command::Open => {
                        let _ = window_handle.update(cx, |_, window, _| {
                            window.activate_window();
                        });
                    }
                    tray::Command::Quit => {
                        // Prototype: tray Quit ends the process. The integrated
                        // frontend will route this through the application quit
                        // path instead.
                        std::process::exit(0);
                    }
                }
            }
        })
        .detach();

        cx.activate(true);
    });
    Ok(())
}
