mod imp;

use std::collections::HashMap;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::{Object, clone};
use gtk::{
    NoSelection, gio,
    glib::{self, closure_local},
};

use lan_mouse_ipc::{
    Capabilities, ClientConfig, ClientHandle, ClientState, ClipboardStatus, DEFAULT_PORT,
    DeviceView, DirectionPolicy, DiscoveredDevice, FrontendRequest, FrontendRequestWriter,
    IPC_VERSION, PendingView, Position,
};

use crate::{
    authorization_window::AuthorizationWindow, fingerprint_window::FingerprintWindow,
    key_object::KeyObject, key_row::KeyRow,
};

use self::imp::DeviceControls;
use super::{client_object::ClientObject, client_row::ClientRow};

/// Capabilities in the exact order their checkboxes are rendered.
const CAPABILITIES: [Capabilities; 3] = [
    Capabilities::POINTER,
    Capabilities::KEYBOARD,
    Capabilities::CLIPBOARD,
];

/// Rebuild a capability mask from the three checkboxes of one direction.
fn caps_from_checks(checks: &[gtk::CheckButton; 3]) -> Capabilities {
    let mut bits = 0u8;
    for (check, capability) in checks.iter().zip(CAPABILITIES) {
        if check.is_active() {
            bits |= capability.0;
        }
    }
    Capabilities(bits)
}

/// A direction is only allowed when at least one capability is granted;
/// unchecking everything therefore means "blocked".
fn policy_from_checks(checks: &[gtk::CheckButton; 3]) -> DirectionPolicy {
    let capabilities = caps_from_checks(checks);
    DirectionPolicy {
        allowed: !capabilities.is_empty(),
        capabilities,
    }
}

/// Rebuild a direction policy from the bitmask reported by the authorization
/// window (0 means the direction was not granted at all).
fn policy_from_mask(mask: u32) -> DirectionPolicy {
    let capabilities = Capabilities(mask as u8 & Capabilities::ALL.0);
    DirectionPolicy {
        allowed: !capabilities.is_empty(),
        capabilities,
    }
}

/// Short form of a SHA-256 fingerprint for row titles.
///
/// The identity stays the full fingerprint (stored in `DeviceView`); the title
/// only needs enough prefix to tell devices apart at a glance.
fn short_fingerprint(fingerprint: &str) -> String {
    const SHORT_LEN: usize = 17;
    let mut short: String = fingerprint.chars().take(SHORT_LEN).collect();
    if fingerprint.chars().count() > SHORT_LEN {
        short.push('…');
    }
    short
}

/// One line status for a device row.
fn device_subtitle(device: &DeviceView) -> String {
    let state = if device.connected && device.active {
        "Receiving"
    } else if device.connected {
        "Connected"
    } else {
        "Offline"
    };
    let incoming = if device.incoming.allowed {
        device.incoming.capabilities.summary()
    } else {
        "blocked".to_string()
    };
    let outgoing = if device.outgoing.allowed {
        device.outgoing.capabilities.summary()
    } else {
        "blocked".to_string()
    };
    let mut subtitle = format!(
        "{state} · {} · controls me: {incoming} · I control it: {outgoing}",
        device.position
    );
    if let Some(address) = device.addresses.first() {
        subtitle.push_str(&format!(" · {address}:{}", device.port));
    }
    subtitle
}

/// Build an action row with a single capability checkbox as its suffix.
fn capability_row(title: &str, subtitle: &str, active: bool) -> (adw::ActionRow, gtk::CheckButton) {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(title))
        .subtitle(glib::markup_escape_text(subtitle))
        .build();
    let check = gtk::CheckButton::new();
    check.set_active(active);
    check.set_valign(gtk::Align::Center);
    row.add_suffix(&check);
    row.set_activatable_widget(Some(&check));
    (row, check)
}

/// Section header inside an expander row.
fn direction_header_row(title: &str, subtitle: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(glib::markup_escape_text(title))
        .subtitle(glib::markup_escape_text(subtitle))
        .build()
}

#[cfg(target_os = "macos")]
fn set_button_content_label(button: &gtk::Button, label: &str) {
    // The Reenable/Grant/Relaunch button wraps its icon+label in an
    // AdwButtonContent (see window.ui). Walk into it and swap the label
    // rather than GtkButton::set_label, which would replace the content
    // widget and drop the icon.
    if let Some(content) = button.child().and_downcast::<adw::ButtonContent>() {
        content.set_label(label);
    }
}

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends adw::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl Window {
    pub(super) fn new(app: &adw::Application, conn: FrontendRequestWriter) -> Self {
        let window: Self = Object::builder().property("application", app).build();
        window
            .imp()
            .frontend_request_writer
            .borrow_mut()
            .replace(conn);
        // Populate the network table without waiting for the user to press
        // Scan; the service answers with `FrontendEvent::NetworkScan`.
        window.imp().trusted_entry.connect_activate(clone!(
            #[weak]
            window,
            move |entry| {
                let address = entry.text().to_string();
                if !address.trim().is_empty() {
                    window.trust_address(address);
                    entry.set_text("");
                }
            }
        ));
        window.imp().network_search.connect_search_changed(clone!(
            #[weak]
            window,
            move |_| window.render_network()
        ));
        // Announce the IPC version before anything else: the service replies
        // with `FrontendEvent::Hello` and a skew is surfaced in the UI instead
        // of silently mis-decoding later events.
        window.request(FrontendRequest::Hello {
            ipc_version: IPC_VERSION,
        });
        // Device permissions and unauthorized connection requests are served
        // as idempotent snapshots, so ask for both right away instead of
        // waiting for the service to push them.
        window.request(FrontendRequest::ListDevices);
        window.request(FrontendRequest::ListPending);
        window.request_scan();

        let clipboard_row = adw::ActionRow::new();
        clipboard_row.set_title("Clipboard");
        clipboard_row.set_subtitle("Clipboard sync is disabled");
        let clipboard_switch = gtk::Switch::new();
        clipboard_switch.set_valign(gtk::Align::Center);
        clipboard_switch.set_active(false);
        clipboard_row.add_suffix(&clipboard_switch);
        clipboard_row.set_activatable_widget(Some(&clipboard_switch));
        clipboard_switch.connect_active_notify(clone!(
            #[weak(rename_to = window)]
            window,
            move |switch| {
                if window.imp().updating_clipboard.get() {
                    return;
                }
                window.request(FrontendRequest::SetClipboardEnabled(switch.is_active()));
            }
        ));
        // The clipboard toggle is global (not tied to capture/emulation), so
        // it lives in its own group: `capture_emulation_group` is hidden on
        // the happy path and would take the toggle with it.
        window.imp().clipboard_group.add(&clipboard_row);
        window
            .imp()
            .clipboard_row
            .replace(Some((clipboard_row, clipboard_switch)));
        window
    }

    pub(super) fn set_clipboard_status(&self, status: ClipboardStatus) {
        let Some((row, switch)) = self.imp().clipboard_row.borrow().as_ref().cloned() else {
            return;
        };
        self.imp().updating_clipboard.set(true);
        switch.set_active(status.enabled);
        self.imp().updating_clipboard.set(false);
        row.set_subtitle(&status.summary());
    }

    fn clients(&self) -> gio::ListStore {
        self.imp()
            .clients
            .borrow()
            .clone()
            .expect("Could not get clients")
    }

    fn authorized(&self) -> gio::ListStore {
        self.imp()
            .authorized
            .borrow()
            .clone()
            .expect("Could not get authorized")
    }

    fn client_by_idx(&self, idx: u32) -> Option<ClientObject> {
        self.clients().item(idx).map(|o| o.downcast().unwrap())
    }

    fn authorized_by_idx(&self, idx: u32) -> Option<KeyObject> {
        self.authorized().item(idx).map(|o| o.downcast().unwrap())
    }

    fn row_by_idx(&self, idx: i32) -> Option<ClientRow> {
        self.imp()
            .client_list
            .get()
            .row_at_index(idx)
            .map(|o| o.downcast().expect("expected ClientRow"))
    }

    fn setup_authorized(&self) {
        let store = gio::ListStore::new::<KeyObject>();
        self.imp().authorized.replace(Some(store));
        let selection_model = NoSelection::new(Some(self.authorized()));
        self.imp().authorized_list.bind_model(
            Some(&selection_model),
            clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or_panic]
                move |obj| {
                    let key_obj = obj.downcast_ref().expect("object of type `KeyObject`");
                    let row = window.create_key_row(key_obj);
                    row.connect_closure(
                        "request-delete",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: KeyRow| {
                                if let Some(key_obj) = window.authorized_by_idx(row.index() as u32)
                                {
                                    window.request_fingerprint_remove(key_obj.get_fingerprint());
                                }
                            }
                        ),
                    );
                    row.upcast()
                }
            ),
        )
    }

    fn setup_clients(&self) {
        let model = gio::ListStore::new::<ClientObject>();
        self.imp().clients.replace(Some(model));

        let selection_model = NoSelection::new(Some(self.clients()));
        self.imp().client_list.bind_model(
            Some(&selection_model),
            clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or_panic]
                move |obj| {
                    let client_object = obj
                        .downcast_ref()
                        .expect("Expected object of type `ClientObject`.");
                    let row = window.create_client_row(client_object);
                    row.connect_closure(
                        "request-hostname-change",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, hostname: String| {
                                log::debug!("request-hostname-change");
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    let hostname = Some(hostname).filter(|s| !s.is_empty());
                                    /* changed in response to FrontendEvent
                                     * -> do not request additional update */
                                    window.request(FrontendRequest::UpdateHostname(
                                        client.handle(),
                                        hostname,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-port-change",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, port: u32| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::UpdatePort(
                                        client.handle(),
                                        port as u16,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-activate",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, active: bool| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    log::debug!(
                                        "request: {} client",
                                        if active { "activating" } else { "deactivating" }
                                    );
                                    window.request(FrontendRequest::Activate(
                                        client.handle(),
                                        active,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-delete",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::Delete(client.handle()));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-dns",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::ResolveDns(
                                        client.get_data().handle,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-sensitivity-change",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, percent: u32| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::UpdateSensitivity(
                                        client.handle(),
                                        percent.min(u16::MAX as u32) as u16,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-position-change",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, pos_idx: u32| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    let position = match pos_idx {
                                        0 => Position::Left,
                                        1 => Position::Right,
                                        2 => Position::Top,
                                        _ => Position::Bottom,
                                    };
                                    window.request(FrontendRequest::UpdatePosition(
                                        client.handle(),
                                        position,
                                    ));
                                }
                            }
                        ),
                    );
                    row.upcast()
                }
            ),
        );
    }

    fn setup_icon(&self) {
        self.set_icon_name(Some("de.feschber.LanMouse"));
    }

    /// workaround for a bug in libadwaita that shows an ugly line beneath
    /// the last element if a placeholder is set.
    /// https://gitlab.gnome.org/GNOME/gtk/-/merge_requests/6308
    fn update_placeholder_visibility(&self) {
        let visible = self.clients().n_items() == 0;
        let placeholder = self.imp().client_placeholder.get();
        self.imp().client_list.set_placeholder(match visible {
            true => Some(&placeholder),
            false => None,
        });
    }

    fn update_auth_placeholder_visibility(&self) {
        let visible = self.authorized().n_items() == 0;
        let placeholder = self.imp().authorized_placeholder.get();
        self.imp().authorized_list.set_placeholder(match visible {
            true => Some(&placeholder),
            false => None,
        });
    }

    fn create_client_row(&self, client_object: &ClientObject) -> ClientRow {
        let row = ClientRow::new(client_object);
        row.bind(client_object);
        row
    }

    fn create_key_row(&self, key_object: &KeyObject) -> KeyRow {
        let row = KeyRow::new();
        row.bind(key_object);
        row
    }

    pub(super) fn new_client(
        &self,
        handle: ClientHandle,
        client: ClientConfig,
        state: ClientState,
    ) {
        let client = ClientObject::new(handle, client, state.clone());
        self.clients().append(&client);
        self.update_placeholder_visibility();
        self.update_dns_state(handle, !state.ips.is_empty());

        // A client created by the "add device" dialog carries its address and
        // side; apply them as soon as the service reports the handle.
        if let Some(imp::PendingAdd {
            address,
            hostname: known_hostname,
            position,
            fingerprint,
        }) = self.imp().pending_add.borrow_mut().take()
        {
            let address = address.trim().to_string();
            let display_name = known_hostname
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| address.clone());
            self.request(FrontendRequest::UpdatePosition(handle, position));
            // A discovered Lan Mouse device already told us its certificate:
            // authorize it here so the other machine does not have to ask.
            // The address itself is *not* added to the trusted allowlist:
            // auto-trusting an IP advertised over unauthenticated UDP would
            // let any host on that address in without the fingerprint check.
            if let Some(fingerprint) = fingerprint {
                self.request(FrontendRequest::AuthorizeKey(
                    display_name.clone(),
                    fingerprint,
                ));
            }
            match address.parse::<std::net::IpAddr>() {
                Ok(ip) => {
                    self.request(FrontendRequest::UpdateHostname(handle, Some(display_name)));
                    self.request(FrontendRequest::UpdateFixIps(handle, vec![ip]));
                }
                Err(_) if !address.is_empty() => {
                    self.request(FrontendRequest::UpdateHostname(handle, Some(address)))
                }
                Err(_) => {}
            }
            self.request(FrontendRequest::Activate(handle, true));
            self.show_toast("Request sent — approve it on the other device");
        }
    }

    pub(super) fn update_client_list(
        &self,
        clients: Vec<(ClientHandle, ClientConfig, ClientState)>,
    ) {
        for (handle, client, state) in clients {
            if self.client_idx(handle).is_some() {
                self.update_client_config(handle, client);
                self.update_client_state(handle, state);
            } else {
                self.new_client(handle, client, state);
            }
        }
    }

    pub(super) fn update_port(&self, port: u16, msg: Option<String>) {
        if let Some(msg) = msg {
            self.show_toast(msg.as_str());
        }
        self.imp().set_port(port);
    }

    fn client_idx(&self, handle: ClientHandle) -> Option<usize> {
        self.clients()
            .iter::<ClientObject>()
            .position(|c| c.ok().map(|c| c.handle() == handle).unwrap_or_default())
    }

    pub(super) fn delete_client(&self, handle: ClientHandle) {
        let Some(idx) = self.client_idx(handle) else {
            log::warn!("could not find client with handle {handle}");
            return;
        };

        self.clients().remove(idx as u32);
        if self.clients().n_items() == 0 {
            self.update_placeholder_visibility();
        }
    }

    pub(super) fn update_client_config(&self, handle: ClientHandle, client: ClientConfig) {
        let Some(row) = self.row_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };
        row.set_hostname(client.hostname);
        row.set_port(client.port);
        row.set_position(client.pos);
        row.set_sensitivity(client.sensitivity);
        row.refresh_status();
    }

    pub(super) fn update_client_state(&self, handle: ClientHandle, state: ClientState) {
        let Some(row) = self.row_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };
        let Some(client_object) = self.client_object_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };

        /* activation state (drives the derived row state: badge, spinner, action) */
        client_object.set_active(state.active);

        /* dns state */
        client_object.set_resolving(state.resolving);

        self.update_dns_state(handle, !state.ips.is_empty());
        let ips = state
            .ips
            .into_iter()
            .map(|ip| ip.to_string())
            .collect::<Vec<_>>();
        client_object.set_ips(ips);

        /* peer build version (drives the version-match indicator) */
        client_object.set_property(
            "peer-commit",
            crate::client_object::peer_commit_to_string(state.peer_commit),
        );
        client_object.set_property("rtt-ms", state.rtt_ms.unwrap_or(0));
        client_object.set_property("alive", state.alive);
        client_object.set_property(
            "active-addr",
            state.active_addr.map(|addr| addr.to_string()),
        );
        client_object.set_property(
            "incoming-addr",
            state.incoming_addr.map(|addr| addr.to_string()),
        );
        row.refresh_status();
    }

    fn client_object_for_handle(&self, handle: ClientHandle) -> Option<ClientObject> {
        self.client_idx(handle)
            .and_then(|i| self.client_by_idx(i as u32))
    }

    fn row_for_handle(&self, handle: ClientHandle) -> Option<ClientRow> {
        self.client_idx(handle)
            .and_then(|i| self.row_by_idx(i as i32))
    }

    fn update_dns_state(&self, handle: ClientHandle, resolved: bool) {
        if let Some(client_row) = self.row_for_handle(handle) {
            client_row.set_dns_state(resolved);
        }
    }

    fn request_port_change(&self) {
        let port = self
            .imp()
            .port_entry
            .get()
            .text()
            .as_str()
            .parse::<u16>()
            .unwrap_or(DEFAULT_PORT);
        self.request(FrontendRequest::ChangePort(port));
    }

    fn request_capture(&self) {
        self.request(FrontendRequest::EnableCapture);
    }

    fn request_emulation(&self) {
        self.request(FrontendRequest::EnableEmulation);
    }

    /// Open the pairing window: any device may connect for `seconds`.
    ///
    /// The toast and the button state are driven by the service's
    /// `FrontendEvent::PairingActive` reply (see [`Self::set_pairing_active`]),
    /// so pairing opened from the CLI is reflected in the GUI too.
    pub(super) fn start_pairing(&self, seconds: u64) {
        self.request(FrontendRequest::StartPairing(seconds));
    }

    /// Reflect whether a pairing window is currently open.
    ///
    /// The service owns the (single) pairing timer, so while it is active the
    /// button is disabled and relabelled; closing or expiry restores it.
    pub(super) fn set_pairing_active(&self, active: bool) {
        let button = &self.imp().pairing_button;
        button.set_sensitive(!active);
        button.set_label(if active { "Pairing…" } else { "Open (60s)" });
    }

    /// Add an address to the allowlist.
    pub(super) fn trust_address(&self, address: String) {
        self.request(FrontendRequest::TrustAddress(address));
    }

    /// Render the allowlist.
    pub(super) fn update_trusted_addresses(&self, addresses: Vec<String>) {
        let list = &self.imp().trusted_list;
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        for address in addresses {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&address))
                .build();
            let delete = gtk::Button::from_icon_name("user-trash-symbolic");
            delete.add_css_class("flat");
            delete.set_tooltip_text(Some("Remove this address"));
            let parent = self.clone();
            let address = address.clone();
            delete.connect_clicked(move |_| {
                parent.request(FrontendRequest::UntrustAddress(address.clone()));
            });
            row.add_suffix(&delete);
            list.append(&row);
        }
    }

    /// Ask the service for a fresh scan of the local network.
    pub(super) fn request_scan(&self) {
        self.request(FrontendRequest::ScanNetwork);
    }

    /// Render the network table.
    pub(super) fn update_network(&self, devices: Vec<DiscoveredDevice>) {
        self.imp().network_devices.replace(devices);
        self.render_network();
    }

    fn render_network(&self) {
        let list = &self.imp().network_list;
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }

        let query = self.imp().network_search.text().to_string().to_lowercase();
        let query = query.trim();
        let devices: Vec<DiscoveredDevice> = self
            .imp()
            .network_devices
            .borrow()
            .iter()
            .filter(|device| {
                if query.is_empty() {
                    return true;
                }
                device.ip.to_string().contains(query)
                    || device
                        .hostname
                        .as_deref()
                        .is_some_and(|name| name.to_lowercase().contains(query))
            })
            .cloned()
            .collect();

        for device in devices {
            let title = device
                .hostname
                .clone()
                .unwrap_or_else(|| device.ip.to_string());
            let mut details = vec![device.ip.to_string(), device.link.to_string()];
            if let Some(version) = &device.version {
                details.push(format!("v{version}"));
            }
            if device.configured {
                details.push("configured".to_string());
            }
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&title))
                .subtitle(glib::markup_escape_text(&details.join(" · ")))
                .build();

            let icon = gtk::Image::from_icon_name(device.link.icon_name());
            icon.set_tooltip_text(Some(match device.link {
                lan_mouse_ipc::LinkKind::Wired => "reached over a cable",
                lan_mouse_ipc::LinkKind::Wireless => "reached over Wi-Fi",
                lan_mouse_ipc::LinkKind::Unknown => "link type unknown",
            }));
            row.add_prefix(&icon);

            if device.lan_mouse {
                let badge = gtk::Image::from_icon_name("emblem-ok-symbolic");
                badge.add_css_class("success");
                badge.set_tooltip_text(Some("Lan Mouse is running on this host"));
                row.add_prefix(&badge);
            }

            if !device.configured {
                let add = gtk::Button::from_icon_name("list-add-symbolic");
                add.add_css_class("flat");
                add.set_tooltip_text(Some("Add this device"));
                let parent = self.clone();
                let address = device.ip.to_string();
                let hostname = device.hostname.clone();
                let fingerprint = device.fingerprint.clone();
                add.connect_clicked(move |_| {
                    parent.request_add_device_with(
                        Some(address.clone()),
                        hostname.clone(),
                        fingerprint.clone(),
                    )
                });
                row.add_suffix(&add);

                let probe = gtk::Button::from_icon_name("system-search-symbolic");
                probe.add_css_class("flat");
                probe.set_tooltip_text(Some("Test whether Lan Mouse answers on this host"));
                let parent = self.clone();
                let ip = device.ip;
                let port = device.port.unwrap_or(DEFAULT_PORT);
                probe.connect_clicked(move |_| parent.confirm_probe(ip, port));
                row.add_suffix(&probe);
            }

            list.append(&row);
        }
    }

    // ----------------------------------------------------------------------
    // Device permissions (`FrontendEvent::Devices`)
    // ----------------------------------------------------------------------

    /// Apply a device snapshot from the service.
    pub(super) fn update_devices(&self, devices: Vec<DeviceView>) {
        self.imp().devices.replace(devices);
        self.render_devices();
    }

    fn render_devices(&self) {
        let group = &self.imp().devices_group;
        let mut rendered = self.imp().device_rows.borrow_mut();
        for row in rendered.drain(..) {
            group.remove(&row);
        }
        self.imp().device_controls.borrow_mut().clear();

        let devices = self.imp().devices.borrow().clone();
        if devices.is_empty() {
            group.set_visible(false);
            return;
        }
        group.set_visible(true);

        let expanded = self.imp().expanded_devices.borrow().clone();
        // Writing the checkbox states below must not be mistaken for a user
        // edit (which would send the policy right back to the service).
        self.imp().updating_devices.set(true);
        for device in &devices {
            let row = self.create_device_row(device);
            // Snapshots are re-rendered on every policy change, so restore the
            // expansion state instead of collapsing what the user just opened.
            row.set_expanded(expanded.contains(&device.fingerprint));
            group.add(&row);
            rendered.push(row);
        }
        self.imp().updating_devices.set(false);
    }

    /// One `AdwExpanderRow` per device: name, state and the permission matrix.
    fn create_device_row(&self, device: &DeviceView) -> adw::ExpanderRow {
        let fingerprint = device.fingerprint.clone();
        let display_name = match device.name.trim() {
            "" => "Unknown device".to_string(),
            name => name.to_string(),
        };

        let row = adw::ExpanderRow::new();
        row.set_title(&glib::markup_escape_text(&format!(
            "{display_name} · {}",
            short_fingerprint(&fingerprint)
        )));
        row.set_subtitle(&glib::markup_escape_text(&device_subtitle(device)));

        let icon = gtk::Image::from_icon_name(if device.connected {
            "network-wired-symbolic"
        } else {
            "network-offline-symbolic"
        });
        icon.set_tooltip_text(Some(if device.connected {
            "connected"
        } else {
            "offline"
        }));
        row.add_prefix(&icon);

        // Direction presets: one click sets all six capability checkboxes.
        let presets = adw::ActionRow::builder()
            .title("Quick setup")
            .subtitle("Presets are shortcuts; the checkboxes below are the stored policy.")
            .build();
        let preset_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        preset_buttons.set_halign(gtk::Align::Start);
        let mut presets_to_connect = Vec::new();
        for (label, incoming, outgoing) in [
            ("Both", true, true),
            ("Only controls me", true, false),
            ("Only I control it", false, true),
        ] {
            let button = gtk::Button::with_label(label);
            button.add_css_class("flat");
            button.add_css_class("pill");
            preset_buttons.append(&button);
            presets_to_connect.push((button, incoming, outgoing));
        }
        presets.set_property("child", &preset_buttons);
        row.add_row(&presets);

        // What the device may do on this computer (authoritative, local).
        row.add_row(&direction_header_row(
            "Can control this PC",
            "What this device may do on this computer. Unchecking everything blocks it.",
        ));
        let (in_pointer_row, in_pointer) = capability_row(
            "Pointer",
            "move the cursor, click and drag",
            device.incoming.allows(Capabilities::POINTER),
        );
        let (in_keyboard_row, in_keyboard) = capability_row(
            "Keyboard",
            "type and use keyboard shortcuts",
            device.incoming.allows(Capabilities::KEYBOARD),
        );
        let (in_clipboard_row, in_clipboard) = capability_row(
            "Clipboard",
            "exchange clipboard contents",
            device.incoming.allows(Capabilities::CLIPBOARD),
        );
        row.add_row(&in_pointer_row);
        row.add_row(&in_keyboard_row);
        row.add_row(&in_clipboard_row);

        // What this computer may do on the device (local guard).
        row.add_row(&direction_header_row(
            "This PC can control it",
            "What this computer may do on the device. The device still enforces its own policy.",
        ));
        let (out_pointer_row, out_pointer) = capability_row(
            "Pointer",
            "move the other device's cursor",
            device.outgoing.allows(Capabilities::POINTER),
        );
        let (out_keyboard_row, out_keyboard) = capability_row(
            "Keyboard",
            "type on the other device",
            device.outgoing.allows(Capabilities::KEYBOARD),
        );
        let (out_clipboard_row, out_clipboard) = capability_row(
            "Clipboard",
            "exchange clipboard contents",
            device.outgoing.allows(Capabilities::CLIPBOARD),
        );
        row.add_row(&out_pointer_row);
        row.add_row(&out_keyboard_row);
        row.add_row(&out_clipboard_row);

        let rename = adw::ActionRow::builder()
            .title("Rename")
            .subtitle("Change the display name. The fingerprint stays the identity.")
            .build();
        let rename_button = gtk::Button::with_label("Rename");
        rename_button.add_css_class("flat");
        rename.add_suffix(&rename_button);
        row.add_row(&rename);

        let revoke = adw::ActionRow::builder()
            .title("Revoke")
            .subtitle("Deny this device and close sessions that are currently open.")
            .build();
        let revoke_button = gtk::Button::with_label("Revoke");
        revoke_button.add_css_class("pill");
        revoke_button.add_css_class("destructive-action");
        revoke.add_suffix(&revoke_button);
        row.add_row(&revoke);

        let controls = DeviceControls {
            incoming: [in_pointer, in_keyboard, in_clipboard],
            outgoing: [out_pointer, out_keyboard, out_clipboard],
        };
        self.imp()
            .device_controls
            .borrow_mut()
            .insert(fingerprint.clone(), controls.clone());

        for check in controls.incoming.iter().chain(controls.outgoing.iter()) {
            let window = self.downgrade();
            let fingerprint = fingerprint.clone();
            check.connect_toggled(move |_| {
                let Some(window) = window.upgrade() else {
                    return;
                };
                if window.imp().updating_devices.get() {
                    return;
                }
                window.request_device_policy(&fingerprint);
            });
        }

        for (button, incoming, outgoing) in presets_to_connect {
            let window = self.downgrade();
            let fingerprint = fingerprint.clone();
            button.connect_clicked(move |_| {
                let Some(window) = window.upgrade() else {
                    return;
                };
                window.apply_device_preset(&fingerprint, incoming, outgoing);
            });
        }

        let parent = self.clone();
        let rename_fingerprint = fingerprint.clone();
        let current_name = device.name.clone();
        rename_button.connect_clicked(move |_| {
            parent.open_rename_device_dialog(&rename_fingerprint, &current_name);
        });

        let parent = self.clone();
        let revoke_fingerprint = fingerprint.clone();
        let revoke_name = display_name.clone();
        revoke_button.connect_clicked(move |_| {
            parent.confirm_revoke_device(&revoke_fingerprint, &revoke_name);
        });

        let window = self.downgrade();
        let expanded_fingerprint = fingerprint;
        row.connect_notify_local(Some("expanded"), move |row, _| {
            let Some(window) = window.upgrade() else {
                return;
            };
            if window.imp().updating_devices.get() {
                return;
            }
            let mut expanded = window.imp().expanded_devices.borrow_mut();
            if row.is_expanded() {
                expanded.insert(expanded_fingerprint.clone());
            } else {
                expanded.remove(&expanded_fingerprint);
            }
        });

        row
    }

    /// Send the policy that the row's checkboxes currently describe.
    fn request_device_policy(&self, fingerprint: &str) {
        let Some(controls) = self
            .imp()
            .device_controls
            .borrow()
            .get(fingerprint)
            .cloned()
        else {
            log::warn!("no controls registered for device {fingerprint}");
            return;
        };
        self.request(FrontendRequest::SetDevicePolicy {
            fingerprint: fingerprint.to_string(),
            incoming: policy_from_checks(&controls.incoming),
            outgoing: policy_from_checks(&controls.outgoing),
        });
    }

    /// Apply a direction preset to all checkboxes and persist it once.
    fn apply_device_preset(&self, fingerprint: &str, incoming: bool, outgoing: bool) {
        let Some(controls) = self
            .imp()
            .device_controls
            .borrow()
            .get(fingerprint)
            .cloned()
        else {
            log::warn!("no controls registered for device {fingerprint}");
            return;
        };
        self.imp().updating_devices.set(true);
        for check in &controls.incoming {
            check.set_active(incoming);
        }
        for check in &controls.outgoing {
            check.set_active(outgoing);
        }
        self.imp().updating_devices.set(false);
        self.request_device_policy(fingerprint);
    }

    fn open_rename_device_dialog(&self, fingerprint: &str, current_name: &str) {
        let window = adw::Window::builder()
            .title("Rename device")
            .modal(true)
            .transient_for(self)
            .default_width(420)
            .build();

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);

        let info = gtk::Label::new(Some(
            "The fingerprint stays the device identity; only the name shown in this \
             list and in notifications changes.",
        ));
        info.set_wrap(true);
        info.set_xalign(0.0);

        let entry = gtk::Entry::new();
        entry.set_placeholder_text(Some("Device name"));
        entry.set_text(current_name);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let save = gtk::Button::with_label("Save");
        save.add_css_class("suggested-action");
        buttons.append(&cancel);
        buttons.append(&save);

        content.append(&info);
        content.append(&entry);
        content.append(&buttons);
        window.set_content(Some(&content));

        let close_window = window.clone();
        cancel.connect_clicked(move |_| close_window.close());

        let parent = self.clone();
        let close_window = window.clone();
        let fingerprint = fingerprint.to_string();
        let entry_for_save = entry.clone();
        save.connect_clicked(move |_| {
            let name = entry_for_save.text().trim().to_string();
            if name.is_empty() {
                parent.show_toast("The device name cannot be empty");
                return;
            }
            parent.request(FrontendRequest::RenameDevice {
                fingerprint: fingerprint.clone(),
                name,
            });
            close_window.close();
        });

        entry.connect_activate(glib::clone!(
            #[weak]
            save,
            move |_| save.emit_clicked()
        ));

        window.present();
    }

    fn confirm_revoke_device(&self, fingerprint: &str, name: &str) {
        let window = adw::Window::builder()
            .title("Revoke device")
            .modal(true)
            .transient_for(self)
            .default_width(420)
            .build();

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);

        let info = gtk::Label::new(Some(&format!(
            "Revoke “{name}”?\n\nOpen sessions with this device are closed and it is \
             denied until you authorize it again."
        )));
        info.set_wrap(true);
        info.set_xalign(0.0);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let revoke = gtk::Button::with_label("Revoke");
        revoke.add_css_class("destructive-action");
        buttons.append(&cancel);
        buttons.append(&revoke);

        content.append(&info);
        content.append(&buttons);
        window.set_content(Some(&content));

        let close_window = window.clone();
        cancel.connect_clicked(move |_| close_window.close());

        let parent = self.clone();
        let close_window = window.clone();
        let fingerprint = fingerprint.to_string();
        let name = name.to_string();
        revoke.connect_clicked(move |_| {
            parent.request(FrontendRequest::RevokeDevice {
                fingerprint: fingerprint.clone(),
                disconnect: true,
            });
            parent.show_toast(&format!("{name} revoked"));
            close_window.close();
        });

        window.present();
    }

    // ----------------------------------------------------------------------
    // Connection requests (`FrontendEvent::PendingRequests`)
    // ----------------------------------------------------------------------

    /// Apply a pending-request snapshot from the service.
    pub(super) fn update_pending(&self, pending: Vec<PendingView>) {
        self.imp().pending.replace(pending);
        self.render_pending();
    }

    fn render_pending(&self) {
        let group = &self.imp().requests_group;
        let mut rendered = self.imp().request_rows.borrow_mut();
        for row in rendered.drain(..) {
            group.remove(&row);
        }

        let pending = self.imp().pending.borrow().clone();
        if pending.is_empty() {
            group.set_visible(false);
            return;
        }
        group.set_visible(true);
        group.set_title(&format!("Connection requests ({})", pending.len()));

        for request in pending {
            let name = request
                .name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty());
            let title = name.unwrap_or("Unknown device");
            let subtitle = format!(
                "{} · {} · {} attempt(s)",
                request.addr.ip(),
                short_fingerprint(&request.fingerprint),
                request.attempts
            );
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(title))
                .subtitle(glib::markup_escape_text(&subtitle))
                .build();

            let deny = gtk::Button::with_label("Deny");
            deny.add_css_class("flat");
            let review = gtk::Button::with_label("Review");
            review.add_css_class("pill");
            review.add_css_class("suggested-action");

            let parent = self.clone();
            let fingerprint = request.fingerprint.clone();
            deny.connect_clicked(move |_| {
                parent.respond_pending_denied(&fingerprint);
            });

            let parent = self.clone();
            let fingerprint = request.fingerprint.clone();
            let addr = request.addr;
            let hostname = request.name.clone();
            let sas = request.sas.clone();
            review.connect_clicked(move |_| {
                parent.show_authorization_window(addr, hostname.as_deref(), &fingerprint, &sas);
            });

            row.add_suffix(&deny);
            row.add_suffix(&review);
            group.add(&row);
            rendered.push(row);
        }
    }

    fn respond_pending_denied(&self, fingerprint: &str) {
        self.request(FrontendRequest::RespondPending {
            fingerprint: fingerprint.to_string(),
            accept: false,
            name: None,
            position: None,
            incoming: None,
            outgoing: None,
        });
        self.show_toast("Connection request denied");
    }

    fn confirm_probe(&self, ip: std::net::IpAddr, port: u16) {
        let window = adw::Window::builder()
            .title("Test device")
            .modal(true)
            .transient_for(self)
            .default_width(420)
            .build();

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);

        let info = gtk::Label::new(Some(&format!(
            "Send a test connection to {ip}:{port}?\n\nIf Lan Mouse runs there and does \
             not know this computer yet, that machine will show an authorization request.",
        )));
        info.set_wrap(true);
        info.set_xalign(0.0);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let probe = gtk::Button::with_label("Test");
        probe.add_css_class("suggested-action");
        buttons.append(&cancel);
        buttons.append(&probe);

        content.append(&info);
        content.append(&buttons);
        window.set_content(Some(&content));

        let close_window = window.clone();
        cancel.connect_clicked(move |_| close_window.close());

        let parent = self.clone();
        let close_window = window.clone();
        probe.connect_clicked(move |_| {
            parent.request(FrontendRequest::ProbeDevice { ip, port });
            parent.show_toast(&format!("Testing {ip}:{port} …"));
            close_window.close();
        });

        window.present();
    }

    /// Collect an address and a screen side, then create the client and
    /// activate it. The peer answers with an authorization request, so this
    /// is the whole "send a connection request" flow.
    pub(super) fn request_add_device(&self) {
        self.request_add_device_with(None, None, None);
    }

    pub(super) fn request_add_device_with(
        &self,
        initial_address: Option<String>,
        known_hostname: Option<String>,
        fingerprint: Option<String>,
    ) {
        let window = adw::Window::builder()
            .title("Add device")
            .modal(true)
            .transient_for(self)
            .default_width(420)
            .build();

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);

        let info = gtk::Label::new(Some(
            "Enter the address of the other computer. It will ask you to approve \
             the connection there, and both directions are configured from that \
             single approval.",
        ));
        info.set_wrap(true);
        info.set_xalign(0.0);

        let entry = gtk::Entry::new();
        entry.set_placeholder_text(Some("192.168.0.10 or hostname"));
        if let Some(address) = initial_address {
            entry.set_text(&address);
        }

        let position_label = gtk::Label::new(Some("The other device is on my:"));
        position_label.set_xalign(0.0);
        position_label.set_margin_top(6);

        let positions = gtk::DropDown::from_strings(&["Left", "Right", "Top", "Bottom"]);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        buttons.set_halign(gtk::Align::End);
        buttons.set_margin_top(12);
        let cancel = gtk::Button::with_label("Cancel");
        let send = gtk::Button::with_label("Send request");
        send.add_css_class("suggested-action");
        buttons.append(&cancel);
        buttons.append(&send);

        content.append(&info);
        content.append(&entry);
        content.append(&position_label);
        content.append(&positions);
        content.append(&buttons);
        window.set_content(Some(&content));

        let close_window = window.clone();
        cancel.connect_clicked(move |_| close_window.close());

        let parent = self.clone();
        let close_window = window.clone();
        let address_entry = entry.clone();
        let position_dropdown = positions.clone();
        send.connect_clicked(move |_| {
            let address = address_entry.text().to_string();
            let position = match position_dropdown.selected() {
                1 => Position::Right,
                2 => Position::Top,
                3 => Position::Bottom,
                _ => Position::Left,
            };
            parent.send_device_request(
                address,
                known_hostname.clone(),
                position,
                fingerprint.clone(),
            );
            close_window.close();
        });

        entry.connect_activate(glib::clone!(
            #[weak]
            send,
            move |_| send.emit_clicked()
        ));

        window.present();
    }

    fn send_device_request(
        &self,
        address: String,
        known_hostname: Option<String>,
        position: Position,
        fingerprint: Option<String>,
    ) {
        self.imp().pending_add.replace(Some(imp::PendingAdd {
            address,
            hostname: known_hostname,
            position,
            fingerprint,
        }));
        self.request(FrontendRequest::Create);
    }

    fn open_fingerprint_dialog(&self, fp: Option<String>) {
        let window = FingerprintWindow::new(fp);
        window.set_transient_for(Some(self));
        window.connect_closure(
            "confirm-clicked",
            false,
            closure_local!(
                #[strong(rename_to = parent)]
                self,
                move |w: FingerprintWindow, desc: String, fp: String| {
                    parent.request_fingerprint_add(desc, fp);
                    w.close();
                }
            ),
        );
        window.present();
    }

    fn request_fingerprint_add(&self, desc: String, fp: String) {
        self.request(FrontendRequest::AuthorizeKey(desc, fp));
    }

    fn request_fingerprint_remove(&self, fp: String) {
        self.request(FrontendRequest::RemoveAuthorizedKey(fp));
    }

    fn request(&self, request: FrontendRequest) {
        let mut requester = self.imp().frontend_request_writer.borrow_mut();
        let requester = requester.as_mut().unwrap();
        if let Err(e) = requester.request(request) {
            log::error!("error sending message: {e}");
        };
    }

    pub(super) fn show_toast(&self, msg: &str) {
        let toast = adw::Toast::new(msg);
        self.add_toast(toast);
    }

    pub(super) fn add_toast(&self, toast: adw::Toast) {
        let toast_overlay = &self.imp().toast_overlay;
        toast_overlay.add_toast(toast);
    }

    pub(super) fn set_capture(&self, active: bool) {
        self.imp().capture_active.replace(active);
        self.update_capture_emulation_status();
    }

    pub(super) fn set_emulation(&self, active: bool) {
        self.imp().emulation_active.replace(active);
        self.update_capture_emulation_status();
    }

    #[cfg(target_os = "macos")]
    pub(super) fn refresh_capture_emulation_status(&self) {
        self.update_capture_emulation_status();
    }

    fn update_capture_emulation_status(&self) {
        let capture = self.imp().capture_active.get();
        let emulation = self.imp().emulation_active.get();

        #[cfg(target_os = "macos")]
        {
            // On macOS, capture and emulation share the same TCC gate
            // (Accessibility). Collapse to a single warning row —
            // emulation_status_row stays hidden and capture_status_row
            // doubles as the shared status indicator. Its text and
            // button mutate based on whether we're waiting for AX or
            // waiting for the user to relaunch the app.
            let anything_off = !capture || !emulation;
            self.imp().emulation_status_row.set_visible(false);
            self.imp().capture_status_row.set_visible(anything_off);
            self.imp().capture_emulation_group.set_visible(anything_off);

            if anything_off {
                self.update_macos_warning_row_text();
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            self.imp().capture_status_row.set_visible(!capture);
            self.imp().emulation_status_row.set_visible(!emulation);
            self.imp()
                .capture_emulation_group
                .set_visible(!capture || !emulation);
        }
    }

    #[cfg(target_os = "macos")]
    fn update_macos_warning_row_text(&self) {
        let row = &self.imp().capture_status_row;
        let button = &self.imp().input_capture_button;

        if crate::macos_privacy::accessibility_granted() {
            // AX granted but capture/emulation still off → the daemon
            // subprocess bailed at startup and needs a fresh process to
            // re-initialize with the new grant in place.
            row.set_title("relaunch required");
            row.set_subtitle("Accessibility granted — restart to activate capture and emulation");
            set_button_content_label(button, "Relaunch");
        } else {
            // AX missing → send the user to System Settings.
            row.set_title("input capture is disabled");
            row.set_subtitle("grant Accessibility permission to enable");
            set_button_content_label(button, "Grant");
        }
    }

    pub(super) fn set_authorized_keys(&self, fingerprints: HashMap<String, String>) {
        let authorized = self.authorized();
        // clear list
        authorized.remove_all();
        // insert fingerprints
        for (fingerprint, description) in fingerprints {
            let key_obj = KeyObject::new(description, fingerprint);
            authorized.append(&key_obj);
        }
        self.update_auth_placeholder_visibility();
    }

    pub(super) fn set_pk_fp(&self, fingerprint: &str) {
        self.imp().fingerprint_row.set_subtitle(fingerprint);
    }

    /// Open the authorization dialog for an incoming connection attempt.
    pub(super) fn request_authorization(
        &self,
        addr: std::net::SocketAddr,
        fingerprint: &str,
        sas: &str,
    ) {
        // The discovery table usually knows the host name of the peer.
        let known_hostname = self
            .imp()
            .network_devices
            .borrow()
            .iter()
            .find(|d| d.ip == addr.ip())
            .and_then(|d| d.hostname.clone());
        self.show_authorization_window(addr, known_hostname.as_deref(), fingerprint, sas);
    }

    /// Open (or replace) the authorization dialog.
    ///
    /// Approving sends [`FrontendRequest::RespondPending`] with the direction
    /// and capability matrix chosen in the dialog; cancelling sends the same
    /// request with `accept: false`, so the service can drop the pending entry
    /// instead of leaving it around.
    fn show_authorization_window(
        &self,
        addr: std::net::SocketAddr,
        hostname: Option<&str>,
        fingerprint: &str,
        sas: &str,
    ) {
        if let Some(w) = self.imp().authorization_window.borrow_mut().take() {
            w.close();
        }
        let window = AuthorizationWindow::new(addr, hostname, fingerprint, sas);
        window.set_transient_for(Some(self));
        let peer_name = hostname
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        window.connect_closure(
            "confirm-clicked",
            false,
            closure_local!(
                #[strong(rename_to = parent)]
                self,
                move |w: AuthorizationWindow,
                      fp: String,
                      position: u32,
                      incoming: u32,
                      outgoing: u32| {
                    w.close();
                    let position = match position {
                        1 => Position::Right,
                        2 => Position::Top,
                        3 => Position::Bottom,
                        _ => Position::Left,
                    };
                    parent.request(FrontendRequest::RespondPending {
                        fingerprint: fp,
                        accept: true,
                        name: peer_name.clone(),
                        position: Some(position),
                        incoming: Some(policy_from_mask(incoming)),
                        outgoing: Some(policy_from_mask(outgoing)),
                    });
                    parent.show_toast("Device authorized");
                }
            ),
        );
        window.connect_closure(
            "cancel-clicked",
            false,
            closure_local!(
                #[strong(rename_to = parent)]
                self,
                move |w: AuthorizationWindow, fp: String| {
                    w.close();
                    parent.request(FrontendRequest::RespondPending {
                        fingerprint: fp,
                        accept: false,
                        name: None,
                        position: None,
                        incoming: None,
                        outgoing: None,
                    });
                    parent.show_toast("Connection request denied");
                }
            ),
        );
        window.present();
        self.imp().authorization_window.replace(Some(window));
    }
}

// GTK window creation from a test thread is only reliable on Linux/Windows;
// macOS requires AppKit's main thread, so the smoke test would fail the macOS
// CI job instead of validating the template.
#[cfg(all(test, not(target_os = "macos")))]
mod ui_template_smoke {
    use adw::prelude::*;
    use adw::subclass::prelude::ObjectSubclassIsExt;
    use gtk::glib;

    /// Builds the composite template once to catch unknown properties/ids.
    /// GTK needs an initialized display; skip silently in headless CI.
    #[test]
    fn window_template_instantiates() {
        if gtk::init().is_err() {
            eprintln!("skipping: no GTK display");
            return;
        }
        gtk::gio::resources_register_include!("lan-mouse.gresource").expect("test gresource");
        let window = glib::Object::builder::<super::Window>().build();
        let subtitle = window.imp().local_addresses_row.subtitle();
        assert!(
            subtitle
                .as_deref()
                .is_some_and(|subtitle| !subtitle.is_empty()),
            "local addresses row must have a subtitle"
        );
        drop(window);
    }
}
