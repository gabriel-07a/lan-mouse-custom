use std::collections::HashMap;

use gpui::{
    App, Application, Bounds, Context, Entity, Render, SharedString, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rgb, size,
};
use lan_mouse_ipc::{
    ClientConfig, ClientHandle, ClientState, FrontendEvent, FrontendRequest, FrontendRequestWriter,
    PendingView,
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

#[cfg(not(target_os = "linux"))]
mod tray {
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Command {
        Open,
        Quit,
    }

    // Windows has its own native tray implementation planned; keep the
    // command channel API identical while that port is in progress.
    pub(crate) fn spawn() -> async_channel::Receiver<Command> {
        let (_, rx) = async_channel::unbounded();
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

enum BridgeMessage {
    Writer(FrontendRequestWriter),
    Event(Box<FrontendEvent>),
    Error(String),
}

struct Dashboard {
    writer: Option<FrontendRequestWriter>,
    clients: HashMap<ClientHandle, (ClientConfig, ClientState)>,
    pending: Vec<PendingView>,
    status: String,
    page: Page,
    selected_client: Option<ClientHandle>,
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
            status: "Connecting to Lan Mouse service…".to_string(),
            page: Page::Devices,
            selected_client: None,
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
            Page::Network => self
                .render_placeholder("Network", "Device discovery will live here.")
                .into_any_element(),
            Page::Settings => self
                .render_placeholder(
                    "Settings",
                    "Capture, clipboard and pairing settings will live here.",
                )
                .into_any_element(),
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
            .when(!self.pending.is_empty(), |this| {
                this.child(
                    div()
                        .p_3()
                        .rounded_md()
                        .bg(rgb(0x2b2413))
                        .text_color(rgb(AMBER))
                        .text_sm()
                        .child(format!(
                            "{} connection request(s) awaiting approval",
                            self.pending.len()
                        )),
                )
            })
    }

    fn render_placeholder(&self, title: &'static str, body: &'static str) -> impl IntoElement {
        let _ = title;
        div()
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .child(div().text_sm().text_color(rgb(MUTED)).child(body))
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

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
}
