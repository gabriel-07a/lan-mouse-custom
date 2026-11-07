use std::collections::HashMap;

use gpui::{
    App, Application, Bounds, Context, Entity, Render, SharedString, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rgb, size,
};
use lan_mouse_ipc::{
    AddressRuleView, ClientConfig, ClientHandle, ClientState, ClipboardStatus, DeviceView,
    DirectionPolicy, FrontendEvent, FrontendRequest, FrontendRequestWriter, PendingView, Position,
    Status,
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

#[cfg(target_os = "linux")]
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
                use ksni::blocking::TrayMethods;
                let tray = LanMouseTray { tx };
                match tray.spawn() {
                    Ok(_handle) => loop {
                        std::thread::park();
                    },
                    Err(error) => log::warn!("system tray unavailable: {error}"),
                }
            })
            .expect("spawn tray thread");
        rx
    }
}

#[cfg(windows)]
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
            FrontendEvent::Created(handle, config, state)
            | FrontendEvent::State(handle, config, state) => {
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

    fn add_device(&mut self) {
        self.send(FrontendRequest::Create);
    }

    fn approve_pending(&mut self, fingerprint: String) {
        self.send(FrontendRequest::RespondPending {
            fingerprint,
            accept: true,
            name: None,
            position: Some(Position::Left),
            incoming: Some(DirectionPolicy::allow_all()),
            outgoing: Some(DirectionPolicy::allow_all()),
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
            .on_click(cx.listener(move |this, _, _, _| this.page = page))
    }

    fn render_client(
        &self,
        handle: ClientHandle,
        (config, state): &(ClientConfig, ClientState),
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let mut row = ClientRow::from_state(config, state);
        row.handle = handle;
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
                                .on_click(cx.listener(|this, _, _, _| this.add_device())),
                        ),
                )
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
                            "Approve",
                            GREEN,
                            0x081018,
                            cx,
                            move |this, _, _, _| this.approve_pending(approve_fp.clone()),
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
            .child(
                div()
                    .p_6()
                    .rounded_lg()
                    .bg(rgb(CARD))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(div().text_sm().text_color(rgb(MUTED)).child(
                        "Network scan results will appear here. The IPC scan request is already wired.",
                    )),
            )
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
                            .text_base()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child(format!("Authorized devices ({})", authorized.len())),
                    )
                    .child(div().text_sm().text_color(rgb(MUTED)).child(
                        if authorized.is_empty() {
                            "No authorized devices.".to_string()
                        } else {
                            authorized.join(" · ")
                        },
                    )),
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
                            .child(format!("Allowed addresses ({})", rules.len())),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child(if rules.is_empty() {
                                "No address rules.".to_string()
                            } else {
                                rules.join(" · ")
                            }),
                    ),
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
