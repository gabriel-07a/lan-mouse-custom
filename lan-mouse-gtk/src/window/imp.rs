use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use adw::subclass::prelude::*;
use adw::{ActionRow, PreferencesGroup, ToastOverlay, prelude::*};
use glib::subclass::InitializingObject;
use gtk::glib::clone;
use gtk::{
    Button, CompositeTemplate, Entry, Image, Label, ListBox, Switch, TemplateChild, gdk, gio, glib,
};

use lan_mouse_ipc::{
    ClientConfig, ClientHandle, ClientState, DEFAULT_PORT, DeviceView, DiscoveredDevice,
    FrontendRequestWriter, PendingView, Position,
};

use crate::authorization_window::AuthorizationWindow;

#[derive(Clone, Debug)]
pub struct PendingAdd {
    pub address: String,
    pub hostname: Option<String>,
    pub position: Position,
    pub fingerprint: Option<String>,
}

/// The six capability checkboxes of a device row, keyed by fingerprint.
///
/// The toggled handlers look these up instead of capturing every widget, so a
/// single handler shape can rebuild the whole [`lan_mouse_ipc::DirectionPolicy`]
/// pair from whatever is currently checked.
#[derive(Clone)]
pub struct DeviceControls {
    pub incoming: [gtk::CheckButton; 3],
    pub outgoing: [gtk::CheckButton; 3],
}

/// GUI-side round-trip bookkeeping for one configured client.
///
/// The service is the source of truth for the connection, but until it exposes
/// a `last_pong_at` timestamp the row can only prove that a reply arrived by
/// observing `FrontendEvent::State` updates: the service broadcasts one per
/// received `Pong` (plus a handful of related events such as `Hello` /
/// `AuthResponse`, which are themselves answers). The first `State` after a
/// handshake is the handshake notification itself and therefore not proof.
#[derive(Debug, Default)]
pub struct LinkUi {
    /// Address of the outgoing link this bookkeeping belongs to.
    pub addr: Option<String>,
    /// `State` updates observed for the current address (first = handshake).
    pub updates: u32,
    /// When the last confirmed round-trip was observed.
    pub last_reply: Option<Instant>,
    /// Address currently shown in the row (debounced, see `sticky_endpoint`).
    pub shown_addr: Option<String>,
    /// Address waiting to replace the shown one once it is stable.
    pub candidate_addr: Option<(String, Instant)>,
    /// Last connection state rendered for this peer; used to avoid rewriting
    /// the row every periodic tick when nothing changed.
    pub last_state: Option<crate::client_row::RowConnectionState>,
}

#[derive(CompositeTemplate, Default)]
#[template(resource = "/de/feschber/LanMouse/window.ui")]
pub struct Window {
    #[template_child]
    pub authorized_placeholder: TemplateChild<ActionRow>,
    #[template_child]
    pub fingerprint_row: TemplateChild<ActionRow>,
    #[template_child]
    pub port_edit_apply: TemplateChild<Button>,
    #[template_child]
    pub port_edit_cancel: TemplateChild<Button>,
    #[template_child]
    pub client_list: TemplateChild<ListBox>,
    #[template_child]
    pub client_placeholder: TemplateChild<ActionRow>,
    #[template_child]
    pub requests_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub devices_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub port_entry: TemplateChild<Entry>,
    #[template_child]
    pub hostname_copy_icon: TemplateChild<Image>,
    #[template_child]
    pub hostname_label: TemplateChild<Label>,
    #[template_child]
    pub local_addresses_row: TemplateChild<ActionRow>,
    #[template_child]
    pub toast_overlay: TemplateChild<ToastOverlay>,
    #[template_child]
    pub capture_emulation_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub clipboard_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub capture_status_row: TemplateChild<ActionRow>,
    #[template_child]
    pub emulation_status_row: TemplateChild<ActionRow>,
    #[template_child]
    pub input_emulation_button: TemplateChild<Button>,
    #[template_child]
    pub input_capture_button: TemplateChild<Button>,
    #[template_child]
    pub pairing_button: TemplateChild<Button>,
    #[template_child]
    pub authorized_list: TemplateChild<ListBox>,
    pub clients: RefCell<Option<gio::ListStore>>,
    pub authorized: RefCell<Option<gio::ListStore>>,
    pub frontend_request_writer: RefCell<Option<FrontendRequestWriter>>,
    pub port: Cell<u16>,
    pub capture_active: Cell<bool>,
    pub emulation_active: Cell<bool>,
    pub clipboard_row: RefCell<Option<(ActionRow, Switch)>>,
    pub updating_clipboard: Cell<bool>,
    pub authorization_window: RefCell<Option<AuthorizationWindow>>,
    #[template_child]
    pub network_list: TemplateChild<gtk::ListBox>,
    #[template_child]
    pub scan_button: TemplateChild<Button>,
    #[template_child]
    pub network_search: TemplateChild<gtk::SearchEntry>,
    #[template_child]
    pub trusted_entry: TemplateChild<Entry>,
    #[template_child]
    pub trusted_list: TemplateChild<gtk::ListBox>,
    /// last scan result, kept so the filter can re-render without rescanning
    pub network_devices: RefCell<Vec<DiscoveredDevice>>,
    /// address, optional hostname, side, and fingerprint collected by the
    /// "add device" dialog, applied to the client entry as soon as the service
    /// reports its handle
    pub pending_add: RefCell<Option<PendingAdd>>,
    /// last `FrontendEvent::Devices` snapshot, rendered as the "Devices" group
    pub devices: RefCell<Vec<DeviceView>>,
    /// checkboxes of each rendered device row, keyed by fingerprint
    pub device_controls: RefCell<HashMap<String, DeviceControls>>,
    /// fingerprints whose expander row is open, preserved across snapshots
    pub expanded_devices: RefCell<HashSet<String>>,
    /// guard so rendering/echoed snapshots do not look like user edits
    pub updating_devices: Cell<bool>,
    /// rows currently inside `devices_group` (the group only knows how to
    /// remove widgets that were added through its own API)
    pub device_rows: RefCell<Vec<adw::ExpanderRow>>,
    /// last `FrontendEvent::PendingRequests` snapshot, rendered as "Requests"
    pub pending: RefCell<Vec<PendingView>>,
    /// rows currently inside `requests_group`
    pub request_rows: RefCell<Vec<adw::ActionRow>>,
    /// last known config and state of every client handle, including entries
    /// that are aggregated into another row (duplicate configured clients for
    /// the same certificate fingerprint).
    pub peer_states: RefCell<HashMap<ClientHandle, (ClientConfig, ClientState)>>,
    /// certificate fingerprint -> client handles of that logical peer.
    pub peer_groups: RefCell<HashMap<String, Vec<ClientHandle>>>,
    /// duplicate handle -> primary (visible) handle of its logical peer.
    pub peer_primary: RefCell<HashMap<ClientHandle, ClientHandle>>,
    /// round-trip freshness bookkeeping per handle.
    pub link_ui: RefCell<HashMap<ClientHandle, LinkUi>>,
    /// handles whose next `State` event was caused by a local config edit
    /// (hostname/port/position/...): such an event must not be mistaken for a
    /// round-trip.
    pub local_config_changes: RefCell<HashSet<ClientHandle>>,
    /// periodical status refresh source, tied to the window lifetime.
    pub status_tick: RefCell<Option<glib::SourceId>>,
}

#[glib::object_subclass]
impl ObjectSubclass for Window {
    // `NAME` needs to match `class` attribute of template
    const NAME: &'static str = "LanMouseWindow";
    const ABSTRACT: bool = false;

    type Type = super::Window;
    type ParentType = adw::ApplicationWindow;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
        klass.bind_template_callbacks();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

#[gtk::template_callbacks]
impl Window {
    #[template_callback]
    fn handle_add_client_pressed(&self, _button: &Button) {
        self.obj().request_add_device();
    }

    #[template_callback]
    fn handle_scan_pressed(&self, _button: &Button) {
        self.obj().request_scan();
    }

    #[template_callback]
    fn handle_start_pairing(&self, _button: &Button) {
        self.obj().start_pairing(60);
    }

    #[template_callback]
    fn handle_trust_address(&self, _button: &Button) {
        let entry = self.trusted_entry.text().to_string();
        if !entry.trim().is_empty() {
            self.obj().trust_address(entry);
            self.trusted_entry.set_text("");
        }
    }

    #[template_callback]
    fn handle_copy_hostname(&self, _: &Button) {
        if let Ok(hostname) = hostname::get() {
            let display = gdk::Display::default().unwrap();
            let clipboard = display.clipboard();
            clipboard.set_text(hostname.to_str().expect("hostname: invalid utf8"));
            let icon = self.hostname_copy_icon.clone();
            icon.set_icon_name(Some("emblem-ok-symbolic"));
            icon.set_css_classes(&["success"]);
            glib::spawn_future_local(clone!(
                #[weak]
                icon,
                async move {
                    glib::timeout_future_seconds(1).await;
                    icon.set_icon_name(Some("edit-copy-symbolic"));
                    icon.set_css_classes(&[]);
                }
            ));
        }
    }

    #[template_callback]
    fn handle_copy_fingerprint(&self, button: &Button) {
        let fingerprint: String = self.fingerprint_row.property("subtitle");
        let display = gdk::Display::default().unwrap();
        let clipboard = display.clipboard();
        clipboard.set_text(&fingerprint);
        button.set_icon_name("emblem-ok-symbolic");
        button.set_css_classes(&["success"]);
        glib::spawn_future_local(clone!(
            #[weak]
            button,
            async move {
                glib::timeout_future_seconds(1).await;
                button.set_icon_name("edit-copy-symbolic");
                button.set_css_classes(&[]);
            }
        ));
    }

    #[template_callback]
    fn handle_copy_local_ips(&self, button: &Button) {
        let addresses = crate::local_addresses::local_addresses();
        let primary = crate::local_addresses::primary_address(&addresses);
        let text = crate::local_addresses::copy_text(primary.as_ref());
        if text.is_empty() {
            self.obj().show_toast("No local IP address to copy");
            return;
        }
        if let Some(display) = gdk::Display::default() {
            display.clipboard().set_text(&text);
            button.set_icon_name("emblem-ok-symbolic");
            button.set_css_classes(&["success"]);
            glib::spawn_future_local(clone!(
                #[weak]
                button,
                async move {
                    glib::timeout_future_seconds(1).await;
                    button.set_icon_name("edit-copy-symbolic");
                    button.set_css_classes(&[]);
                }
            ));
        }
    }

    #[template_callback]
    fn handle_refresh_local_ips(&self, _: &Button) {
        self.refresh_local_addresses();
        self.obj().show_toast("Local IP addresses refreshed");
    }

    #[template_callback]
    fn handle_port_changed(&self, _entry: &Entry) {
        self.port_edit_apply.set_visible(true);
        self.port_edit_cancel.set_visible(true);
    }

    #[template_callback]
    fn handle_port_edit_apply(&self) {
        self.obj().request_port_change();
    }

    #[template_callback]
    fn handle_port_edit_cancel(&self) {
        log::debug!("cancel port edit");
        self.port_entry
            .set_text(self.port.get().to_string().as_str());
        self.port_edit_apply.set_visible(false);
        self.port_edit_cancel.set_visible(false);
    }

    #[template_callback]
    fn handle_emulation(&self) {
        // On macOS the emulation_status_row is hidden — capture_status_row
        // acts as the shared warning (see update_capture_emulation_status).
        // This handler still fires for the non-macOS platforms where the
        // emulation row is distinct.
        self.obj().request_emulation();
    }

    #[template_callback]
    fn handle_capture(&self) {
        #[cfg(target_os = "macos")]
        {
            use crate::macos_privacy;
            if macos_privacy::accessibility_granted() {
                // AX granted but the row is still visible => the daemon
                // subprocess bailed before AX was in place and needs a
                // fresh process.
                log::info!("capture row clicked in relaunch-required state");
                let Some(app) = self.obj().application() else {
                    log::error!("failed to relaunch: window has no application");
                    self.obj().show_toast("Failed to relaunch Lan Mouse");
                    return;
                };
                match macos_privacy::relaunch_bundle() {
                    Ok(()) => app.quit(),
                    Err(error) => {
                        log::error!("failed to schedule app relaunch: {error}");
                        self.obj().show_toast("Failed to relaunch Lan Mouse");
                    }
                }
                return;
            }
            log::info!("capture row clicked in AX-missing state, opening pane");
            macos_privacy::open_accessibility_settings();
        }
        self.obj().request_capture();
    }

    #[template_callback]
    fn handle_add_cert_fingerprint(&self, _button: &Button) {
        self.obj().open_fingerprint_dialog(None);
    }

    pub fn set_port(&self, port: u16) {
        self.port.set(port);
        if port == DEFAULT_PORT {
            self.port_entry.set_text("");
        } else {
            self.port_entry.set_text(format!("{port}").as_str());
        }
        self.port_edit_apply.set_visible(false);
        self.port_edit_cancel.set_visible(false);
        self.refresh_local_addresses();
    }

    /// Re-enumerate this machine's addresses and update the "This Computer"
    /// row. Called on startup, when the listening port changes and by the
    /// refresh button (Wi-Fi/VPN changes are picked up without a restart).
    pub(super) fn refresh_local_addresses(&self) {
        let addresses = crate::local_addresses::local_addresses();
        let primary = crate::local_addresses::primary_address(&addresses);
        let hostname = hostname::get()
            .ok()
            .and_then(|name| name.into_string().ok())
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "This computer".to_string());
        self.local_addresses_row.set_title(&hostname);
        let mut subtitle = crate::local_addresses::subtitle(primary.as_ref());
        if primary.is_some() {
            // The peer needs both values; keep the port one line below the
            // address even though it is also editable under General.
            subtitle.push_str(&format!("\nPort: {}", self.port.get()));
        }
        self.local_addresses_row.set_subtitle(&subtitle);
        self.local_addresses_row.set_tooltip_text(Some(&format!(
            "{}\nPort: {} (editable under General).",
            crate::local_addresses::tooltip(primary.as_ref(), &addresses),
            self.port.get()
        )));
    }
}

impl ObjectImpl for Window {
    fn constructed(&self) {
        if let Ok(hostname) = hostname::get() {
            self.hostname_label
                .set_text(hostname.to_str().expect("hostname: invalid utf8"));
        }
        self.parent_constructed();
        self.set_port(DEFAULT_PORT);
        let obj = self.obj();
        obj.setup_icon();
        obj.setup_clients();
        obj.setup_authorized();
    }
}

impl WidgetImpl for Window {}
impl WindowImpl for Window {}
impl ApplicationWindowImpl for Window {}
impl AdwApplicationWindowImpl for Window {}
