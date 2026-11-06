mod imp;

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
use super::{
    client_object::ClientObject,
    client_row::{
        ClientRow, LinkProof, RowConnectionState, link_proof_from_age, row_connection_state,
        sorted_ips, sticky_endpoint,
    },
};

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

/// A reply younger than this proves that the outgoing link still works.
///
/// The service pings in rounds of ~2 s and closes a link that stopped
/// answering after two missed rounds; 6 s is conservative and absorbs
/// scheduler jitter without flickering the badge on a single lost packet.
const LINK_FRESH_WINDOW: Duration = Duration::from_secs(6);

/// A new endpoint must persist this long before replacing the one shown in the
/// subtitle. Prevents IPv4/IPv6 alternation when the service reports a
/// different source address per packet.
const ENDPOINT_STICKY_WINDOW: Duration = Duration::from_secs(3);

/// Recompute the freshness-dependent parts of every row on this cadence, so a
/// row whose peer goes silent stops claiming "Connected" even when the service
/// stops sending events. 1 s keeps the "no reply for Ns" counter honest.
const STATUS_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Milliseconds since the Unix epoch, matching the clock the service uses for
/// [`lan_mouse_ipc::ClientState::last_pong_at`].
fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

/// Set a GTK object property only when its value differs.
///
/// The status tick refreshes every row once per second; unconditionally
/// writing the properties would emit `notify` every time, and the hostname /
/// port entries are bound to those properties (a `set_text` while the user is
/// typing moves the cursor). Comparing first keeps the tick silent when
/// nothing changed.
fn set_if_changed<T>(object: &ClientObject, property: &str, value: T) -> bool
where
    T: PartialEq + Into<glib::Value> + for<'b> glib::value::FromValue<'b> + 'static,
{
    if object.property::<T>(property) == value {
        return false;
    }
    object.set_property(property, value);
    true
}

/// One logical peer rendered as a single stable row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PeerSummary {
    pub hostname: Option<String>,
    pub port: u16,
    pub position: Position,
    pub sensitivity: u16,
    pub active: bool,
    pub alive: bool,
    pub active_addr: Option<String>,
    pub incoming_addr: Option<String>,
    pub rtt_ms: u32,
    pub peer_commit: Option<String>,
    pub ips: Vec<String>,
    pub resolving: bool,
}

/// Identity of the logical peer a configured client belongs to.
///
/// Only the certificate fingerprint is authoritative: two configured clients
/// with the same fingerprint are the same device (the service bug that creates
/// one entry per address family). Hostnames/IPs are deliberately not used as
/// identity because distinct machines can share them in a misconfigured setup,
/// and merging those rows would hide a real device.
pub(crate) fn peer_identity(config: &ClientConfig) -> Option<String> {
    config
        .fingerprint
        .as_deref()
        .map(str::trim)
        .filter(|fingerprint| !fingerprint.is_empty())
        .map(str::to_ascii_lowercase)
}

/// Merge the configured clients that belong to one logical peer into the single
/// summary its row shows.
///
/// The row must stay stable while the service has duplicate entries for one
/// peer (one per address family): `active` is the OR of the members, and the
/// link fields come from a member with a proven link, preferring one that
/// already has a measured round-trip. Iteration order is the configuration
/// order, so the choice does not flap between updates.
pub(crate) fn merge_peer_states(members: &[(ClientConfig, ClientState)]) -> Option<PeerSummary> {
    let (first_config, _) = members.first()?;
    let link_index = members
        .iter()
        .position(|(_, state)| state.active_addr.is_some() && state.rtt_ms.is_some())
        .or_else(|| {
            members
                .iter()
                .position(|(_, state)| state.active_addr.is_some())
        })
        .unwrap_or(0);
    let (link_config, link_state) = &members[link_index];
    let hostname = members.iter().find_map(|(config, _)| {
        config
            .hostname
            .as_deref()
            .map(str::trim)
            .filter(|hostname| !hostname.is_empty())
            .map(str::to_string)
    });
    let incoming_addr = members
        .iter()
        .find_map(|(_, state)| state.incoming_addr.map(|addr| addr.to_string()));
    let ips = members
        .iter()
        .flat_map(|(_, state)| state.ips.iter().copied())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|ip| ip.to_string())
        .collect();
    Some(PeerSummary {
        hostname,
        port: link_config.port,
        position: link_config.pos,
        sensitivity: first_config.sensitivity,
        active: members.iter().any(|(_, state)| state.active),
        alive: link_state.alive,
        active_addr: link_state.active_addr.map(|addr| addr.to_string()),
        incoming_addr,
        rtt_ms: members
            .iter()
            .filter_map(|(_, state)| state.rtt_ms)
            .max()
            .unwrap_or(0),
        peer_commit: members
            .iter()
            .find_map(|(_, state)| state.peer_commit)
            .and_then(|commit| crate::client_object::peer_commit_to_string(Some(commit))),
        ips,
        resolving: members.iter().any(|(_, state)| state.resolving),
    })
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
    // `DeviceView::addresses` is filled from a `HashSet` in config/service
    // order; sort it so the "Devices" row cannot alternate between IPv4 and
    // IPv6 on every snapshot.
    let mut addresses = device.addresses.clone();
    addresses.sort();
    if let Some(address) = addresses.first() {
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
        // Freshness of the round-trip decays with time; refresh the rows even
        // when the service sends no further events, otherwise a silent peer
        // would keep showing "Connected" forever.
        window.install_status_tick();
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
                                    window.request_peer_hostname(client.handle(), hostname);
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
                                    window.request_peer_port(client.handle(), port as u16);
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
                                    window.request_peer_activate(client.handle(), active);
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
                                    window.request_peer_delete(client.handle());
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
                                    window.request_peer_dns(client.get_data().handle);
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
                                    window.request_peer_sensitivity(
                                        client.handle(),
                                        percent.min(u16::MAX as u32) as u16,
                                    );
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
                                    window.request_peer_position(client.handle(), position);
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

    // ------------------------------------------------------------------
    // Logical peers: round-trip freshness and duplicate aggregation
    // ------------------------------------------------------------------

    /// The visible (primary) handle of the logical peer `handle` belongs to.
    fn peer_primary_of(&self, handle: ClientHandle) -> ClientHandle {
        *self
            .imp()
            .peer_primary
            .borrow()
            .get(&handle)
            .unwrap_or(&handle)
    }

    /// All configured clients that belong to the same logical peer as `handle`.
    fn peer_members(&self, handle: ClientHandle) -> Vec<ClientHandle> {
        let primary = self.peer_primary_of(handle);
        let groups = self.imp().peer_groups.borrow();
        for members in groups.values() {
            if members.contains(&primary) {
                return members.clone();
            }
        }
        vec![primary]
    }

    /// Hide/show the row that belongs to a configured client. Rows are created
    /// by the list factory, so the widget is looked up by index; duplicate
    /// clients stay in the model but their row is hidden.
    fn set_client_row_visible(&self, handle: ClientHandle, visible: bool) {
        let Some(idx) = self.client_idx(handle) else {
            return;
        };
        if let Some(row) = self.imp().client_list.row_at_index(idx as i32) {
            row.set_visible(visible);
        }
    }

    /// Register `handle` in the peer group of its fingerprint and return the
    /// primary (visible) handle of that peer.
    ///
    /// Also removes the handle from a group it used to belong to (a peer whose
    /// certificate was replaced is a different identity); a promoted surviving
    /// member is re-rendered so it does not keep a stale primary.
    fn register_peer_identity(&self, handle: ClientHandle, config: &ClientConfig) -> ClientHandle {
        let Some(identity) = peer_identity(config) else {
            return self.peer_primary_of(handle);
        };
        let mut promoted = None;
        let primary = {
            let mut groups = self.imp().peer_groups.borrow_mut();
            // Drop the handle from any other identity group (e.g. after a
            // certificate change/revocation re-added it under a new pin).
            let stale_identities = groups
                .iter()
                .filter(|(key, members)| {
                    key.as_str() != identity.as_str() && members.contains(&handle)
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            for stale in stale_identities {
                if let Some(members) = groups.get_mut(&stale) {
                    let was_primary = members.first() == Some(&handle);
                    members.retain(|member| *member != handle);
                    if was_primary {
                        promoted = members.first().copied();
                    }
                    if members.is_empty() {
                        groups.remove(&stale);
                    }
                }
            }
            let members = groups.entry(identity).or_default();
            if !members.contains(&handle) {
                members.push(handle);
            }
            members[0]
        };
        {
            let mut primary_map = self.imp().peer_primary.borrow_mut();
            primary_map.retain(|member, primary| *member != handle && *primary != handle);
            if primary != handle {
                primary_map.insert(handle, primary);
            }
        }
        if let Some(promoted) = promoted {
            // Remaining members of the old group now belong to the promoted
            // primary; re-point their mappings so later updates aggregate into
            // the promoted row.
            {
                let groups = self.imp().peer_groups.borrow();
                if let Some(members) = groups.values().find(|members| members.contains(&promoted)) {
                    let mut primary_map = self.imp().peer_primary.borrow_mut();
                    for member in members.iter().copied() {
                        if member == promoted {
                            primary_map.remove(&member);
                        } else {
                            primary_map.insert(member, promoted);
                        }
                    }
                }
            }
            self.set_client_row_visible(promoted, true);
            self.refresh_peer(promoted);
        }
        primary
    }

    /// Store the latest config of a client. Returns `true` when an already
    /// known client's config changed (a local edit or a fingerprint learned
    /// later), which lets the next state update be ignored as round-trip
    /// evidence.
    fn note_config(&self, handle: ClientHandle, config: &ClientConfig) -> bool {
        let mut states = self.imp().peer_states.borrow_mut();
        match states.get_mut(&handle) {
            Some((known, _)) => {
                let changed = known != config;
                *known = config.clone();
                changed
            }
            None => {
                states.insert(handle, (config.clone(), ClientState::default()));
                false
            }
        }
    }

    fn note_state(&self, handle: ClientHandle, state: &ClientState) {
        {
            let mut states = self.imp().peer_states.borrow_mut();
            match states.get_mut(&handle) {
                Some((_, known)) => *known = state.clone(),
                None => {
                    states.insert(handle, (ClientConfig::default(), state.clone()));
                }
            }
        }
        // A `State` event triggered by a local config edit is not a reply from
        // the peer; do not count it as round-trip evidence.
        let local_edit = self.imp().local_config_changes.borrow_mut().remove(&handle);

        // Per-link round-trip bookkeeping (fallback for service builds that do
        // not expose `last_pong_at` yet).
        let addr = state.active_addr.map(|addr| addr.to_string());
        let mut links = self.imp().link_ui.borrow_mut();
        let link = links.entry(handle).or_default();
        if link.addr != addr {
            let previous_addr = link.addr.take();
            link.addr = addr.clone();
            link.updates = 0;
            link.last_reply = None;
            link.candidate_addr = None;
            // The debounced endpoint is kept only while the service keeps
            // reporting a link: a genuine loss or a fresh connection starts
            // from the new address instead of showing the old one for the
            // sticky window.
            if previous_addr.is_none() || addr.is_none() {
                link.shown_addr = None;
            }
        }
        if link.addr.is_some() && !local_edit {
            link.updates = link.updates.saturating_add(1);
            // The first State for a fresh address is the handshake
            // notification itself. Any later update while the link is up means
            // the service produced another event (a Pong or a reply to it).
            if link.updates >= 2 {
                link.last_reply = Some(Instant::now());
            }
        }
    }

    /// Round-trip evidence for one configured client.
    fn link_proof(&self, handle: ClientHandle) -> LinkProof {
        // Prefer the explicit service timestamp when the running service
        // provides it (see docs/audit-connected-truth.md for the service-side
        // patch). It is exact: it is written only on a received Pong.
        let last_pong_at = self
            .imp()
            .peer_states
            .borrow()
            .get(&handle)
            .and_then(|(_, state)| state.last_pong_at);
        if let Some(last_pong_at) = last_pong_at {
            let age = Duration::from_millis(unix_ms_now().saturating_sub(last_pong_at));
            return link_proof_from_age(age, LINK_FRESH_WINDOW);
        }
        // Fallback for service builds without `last_pong_at`: count State
        // updates (one per Pong) and their age.
        let links = self.imp().link_ui.borrow();
        let Some(link) = links.get(&handle) else {
            return LinkProof::Pending;
        };
        if link.updates < 2 {
            return LinkProof::Pending;
        }
        match link.last_reply {
            Some(instant) => link_proof_from_age(instant.elapsed(), LINK_FRESH_WINDOW),
            None => LinkProof::Pending,
        }
    }

    /// Best round-trip evidence across all configured clients of one peer.
    fn merged_proof(&self, primary: ClientHandle) -> LinkProof {
        self.peer_members(primary)
            .into_iter()
            .fold(LinkProof::Pending, |best, handle| {
                best.best(self.link_proof(handle))
            })
    }

    /// Debounce the endpoint shown in the subtitle so a service that reports a
    /// different source address per packet cannot make the row alternate
    /// between IPv4 and IPv6.
    fn stable_endpoint(&self, primary: ClientHandle, raw: Option<String>) -> Option<String> {
        let mut links = self.imp().link_ui.borrow_mut();
        let link = links.entry(primary).or_default();
        sticky_endpoint(
            &mut link.shown_addr,
            &mut link.candidate_addr,
            raw,
            Instant::now(),
            ENDPOINT_STICKY_WINDOW,
        )
    }

    /// Recompute the visible row of one logical peer from all its members,
    /// including the round-trip proof.
    ///
    /// Properties are only written when they changed, and the row is only
    /// re-rendered when the state changed or when the visible text depends on
    /// the current time (`Reconnecting` shows the seconds since the last
    /// reply). This keeps the periodic tick silent for healthy rows.
    fn refresh_peer(&self, primary: ClientHandle) {
        let members = self.peer_members(primary);
        let pairs = {
            let states = self.imp().peer_states.borrow();
            members
                .iter()
                .filter_map(|handle| states.get(handle).cloned())
                .collect::<Vec<_>>()
        };
        let Some(summary) = merge_peer_states(&pairs) else {
            return;
        };
        let Some(client_object) = self.client_object_for_handle(primary) else {
            return;
        };
        let Some(row) = self.row_for_handle(primary) else {
            return;
        };
        let proof = self.merged_proof(primary);
        let state = row_connection_state(
            summary.active,
            summary.alive,
            summary.active_addr.is_some(),
            summary.incoming_addr.is_some(),
            proof,
        );
        let display_addr = self.stable_endpoint(primary, summary.active_addr.clone());
        let mut changed = false;
        changed |= set_if_changed(&client_object, "hostname", summary.hostname);
        changed |= set_if_changed(&client_object, "port", u32::from(summary.port));
        changed |= set_if_changed(&client_object, "position", summary.position.to_string());
        changed |= set_if_changed(
            &client_object,
            "sensitivity",
            u32::from(summary.sensitivity),
        );
        changed |= set_if_changed(&client_object, "active", summary.active);
        changed |= set_if_changed(&client_object, "alive", summary.alive);
        changed |= set_if_changed(&client_object, "active-addr", display_addr);
        changed |= set_if_changed(&client_object, "incoming-addr", summary.incoming_addr);
        changed |= set_if_changed(&client_object, "rtt-ms", summary.rtt_ms);
        changed |= set_if_changed(&client_object, "peer-commit", summary.peer_commit);
        changed |= set_if_changed(&client_object, "ips", sorted_ips(&summary.ips));
        changed |= set_if_changed(&client_object, "resolving", summary.resolving);
        self.update_dns_state(primary, !summary.ips.is_empty());

        let previous_state = self
            .imp()
            .link_ui
            .borrow()
            .get(&primary)
            .and_then(|link| link.last_state);
        let needs_refresh =
            changed || previous_state != Some(state) || state == RowConnectionState::Reconnecting;
        if needs_refresh {
            row.refresh_status_with_proof(proof);
        }
        if let Some(link) = self.imp().link_ui.borrow_mut().get_mut(&primary) {
            link.last_state = Some(state);
        }
    }

    /// Periodic refresh: recompute every visible peer row so freshness decays
    /// even when no event arrives (the only way a silent peer's row can stop
    /// claiming to be connected).
    fn refresh_link_ages(&self) {
        let handles = self
            .imp()
            .peer_states
            .borrow()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let mut primaries: Vec<ClientHandle> = Vec::new();
        for handle in handles {
            let primary = self.peer_primary_of(handle);
            if !primaries.contains(&primary) {
                primaries.push(primary);
            }
        }
        for primary in primaries {
            self.refresh_peer(primary);
        }
    }

    fn install_status_tick(&self) {
        let weak = self.downgrade();
        let source = glib::timeout_add_local(STATUS_TICK_INTERVAL, move || {
            let Some(window) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            window.refresh_link_ages();
            glib::ControlFlow::Continue
        });
        self.imp().status_tick.replace(Some(source));
    }

    // ---- request fan-out: an action on one row applies to the whole peer ----

    fn request_peer_activate(&self, handle: ClientHandle, active: bool) {
        for member in self.peer_members(handle) {
            self.request(FrontendRequest::Activate(member, active));
        }
    }

    fn request_peer_delete(&self, handle: ClientHandle) {
        for member in self.peer_members(handle) {
            self.request(FrontendRequest::Delete(member));
        }
    }

    fn request_peer_hostname(&self, handle: ClientHandle, hostname: Option<String>) {
        for member in self.peer_members(handle) {
            self.request(FrontendRequest::UpdateHostname(member, hostname.clone()));
        }
    }

    fn request_peer_port(&self, handle: ClientHandle, port: u16) {
        for member in self.peer_members(handle) {
            self.request(FrontendRequest::UpdatePort(member, port));
        }
    }

    fn request_peer_position(&self, handle: ClientHandle, position: Position) {
        for member in self.peer_members(handle) {
            self.request(FrontendRequest::UpdatePosition(member, position));
        }
    }

    fn request_peer_sensitivity(&self, handle: ClientHandle, sensitivity: u16) {
        for member in self.peer_members(handle) {
            self.request(FrontendRequest::UpdateSensitivity(member, sensitivity));
        }
    }

    fn request_peer_dns(&self, handle: ClientHandle) {
        for member in self.peer_members(handle) {
            self.request(FrontendRequest::ResolveDns(member));
        }
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
        let client_object = ClientObject::new(handle, client.clone(), state.clone());
        self.clients().append(&client_object);
        self.note_state(handle, &state);
        let _ = self.note_config(handle, &client);

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

        // One logical peer -> one visible row. Duplicate configured clients for
        // the same certificate fingerprint are kept in the model (so their
        // state can be read/aggregated and deleted) but their row is hidden.
        let primary = self.register_peer_identity(handle, &client);
        self.set_client_row_visible(handle, primary == handle);
        self.update_placeholder_visibility();
        self.refresh_peer(primary);
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
        let identity = self
            .imp()
            .peer_states
            .borrow()
            .get(&handle)
            .and_then(|(config, _)| peer_identity(config));

        self.clients().remove(idx as u32);
        self.imp().peer_states.borrow_mut().remove(&handle);
        self.imp().link_ui.borrow_mut().remove(&handle);

        // Drop the handle from its peer group; if it was the visible row of a
        // group, promote a surviving duplicate so the peer stays on screen.
        let mut promoted = None;
        if let Some(identity) = identity {
            let mut groups = self.imp().peer_groups.borrow_mut();
            let mut primary_map = self.imp().peer_primary.borrow_mut();
            let mut empty = None;
            if let Some(members) = groups.get_mut(&identity) {
                members.retain(|member| *member != handle);
                if members.is_empty() {
                    empty = Some(identity.clone());
                } else {
                    let new_primary = members[0];
                    promoted = Some(new_primary);
                    for member in members.iter().copied() {
                        if member == new_primary {
                            primary_map.remove(&member);
                        } else {
                            primary_map.insert(member, new_primary);
                        }
                    }
                }
            }
            if let Some(empty) = empty {
                groups.remove(&empty);
            }
            primary_map.remove(&handle);
        }

        if let Some(promoted) = promoted {
            self.set_client_row_visible(promoted, true);
            self.refresh_peer(promoted);
        }
        if self.clients().n_items() == 0 {
            self.update_placeholder_visibility();
        }
    }

    pub(super) fn update_client_config(&self, handle: ClientHandle, client: ClientConfig) {
        if self.note_config(handle, &client) {
            self.imp().local_config_changes.borrow_mut().insert(handle);
        }
        // The fingerprint can be learned after creation (TOFU/authorization):
        // re-register so two entries for the same peer end up as one row.
        let primary = self.register_peer_identity(handle, &client);
        self.set_client_row_visible(handle, primary == handle);
        self.refresh_peer(primary);
    }

    pub(super) fn update_client_state(&self, handle: ClientHandle, state: ClientState) {
        self.note_state(handle, &state);
        let primary = self.peer_primary_of(handle);
        self.refresh_peer(primary);
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

        let mut devices = self.imp().devices.borrow().clone();
        // Defensive: one row per device identity, ordered by name so a
        // reordered snapshot cannot shuffle the list under the user's cursor.
        devices.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.fingerprint.cmp(&b.fingerprint))
        });
        devices.dedup_by(|a, b| a.fingerprint.eq_ignore_ascii_case(&b.fingerprint));
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

#[cfg(test)]
mod peer_merge_tests {
    use super::*;

    fn config(hostname: Option<&str>, fingerprint: Option<&str>) -> ClientConfig {
        ClientConfig {
            hostname: hostname.map(str::to_string),
            fingerprint: fingerprint.map(str::to_string),
            port: 4242,
            ..Default::default()
        }
    }

    fn state(
        active: bool,
        alive: bool,
        active_addr: Option<&str>,
        incoming_addr: Option<&str>,
        rtt_ms: Option<u32>,
        ips: &[&str],
    ) -> ClientState {
        ClientState {
            active,
            alive,
            active_addr: active_addr.map(|addr| addr.parse().expect("address")),
            incoming_addr: incoming_addr.map(|addr| addr.parse().expect("address")),
            rtt_ms,
            ips: ips
                .iter()
                .map(|ip| ip.parse().expect("ip"))
                .collect::<std::collections::HashSet<_>>(),
            ..Default::default()
        }
    }

    #[test]
    fn peer_identity_requires_a_fingerprint() {
        assert_eq!(
            peer_identity(&config(Some("predator"), Some("AA:BB:CC:DD"))),
            Some("aa:bb:cc:dd".to_string())
        );
        // A hostname is not an identity: two different machines can share one.
        assert_eq!(peer_identity(&config(Some("predator"), None)), None);
        assert_eq!(peer_identity(&config(None, Some("  "))), None);
    }

    #[test]
    fn single_member_summary_is_a_passthrough() {
        let members = [(
            config(Some("predator"), Some("aa:bb")),
            state(
                true,
                true,
                Some("192.168.166.197:4242"),
                None,
                Some(7),
                &["192.168.166.197"],
            ),
        )];
        let summary = merge_peer_states(&members).expect("summary");
        assert_eq!(summary.hostname.as_deref(), Some("predator"));
        assert!(summary.active);
        assert!(summary.alive);
        assert_eq!(summary.port, 4242);
        assert_eq!(summary.active_addr.as_deref(), Some("192.168.166.197:4242"));
        assert_eq!(summary.incoming_addr, None);
        assert_eq!(summary.rtt_ms, 7);
        assert_eq!(summary.ips, vec!["192.168.166.197".to_string()]);
    }

    #[test]
    fn duplicate_entries_of_one_peer_are_merged_into_one_row() {
        // Regression for the reported "two connected devices for the same
        // peer": one configured client per address family, same fingerprint.
        let members = [
            (
                config(Some("predator"), Some("aa:bb")),
                state(
                    true,
                    true,
                    Some("192.168.166.197:4242"),
                    None,
                    Some(4),
                    &["192.168.166.197"],
                ),
            ),
            (
                config(Some("predator"), Some("aa:bb")),
                state(
                    false,
                    false,
                    Some("[2804:2b24:0:6::1]:4242"),
                    Some("[2804:2b24:0:6::1]:50000"),
                    None,
                    &["2804:2b24:0:6::1"],
                ),
            ),
        ];
        let summary = merge_peer_states(&members).expect("summary");
        // OR of the members: the peer is enabled even if only one entry is.
        assert!(summary.active);
        // Link data comes from the member with a proven round-trip, so the
        // summary does not flap between the two links.
        assert_eq!(summary.active_addr.as_deref(), Some("192.168.166.197:4242"));
        assert_eq!(summary.rtt_ms, 4);
        assert!(summary.alive);
        assert_eq!(
            summary.incoming_addr.as_deref(),
            Some("[2804:2b24:0:6::1]:50000")
        );
        // Union of both address lists, stable IPv4-first order.
        assert_eq!(
            summary.ips,
            vec![
                "192.168.166.197".to_string(),
                "2804:2b24:0:6::1".to_string()
            ]
        );
    }

    #[test]
    fn merge_prefers_the_member_with_a_measured_round_trip() {
        let members = [
            (
                config(Some("predator"), Some("aa:bb")),
                state(
                    true,
                    true,
                    Some("192.168.166.197:4242"),
                    None,
                    None,
                    &["192.168.166.197"],
                ),
            ),
            (
                config(Some("predator"), Some("aa:bb")),
                state(
                    true,
                    false,
                    Some("[2804:2b24:0:6::1]:4242"),
                    None,
                    Some(9),
                    &["2804:2b24:0:6::1"],
                ),
            ),
        ];
        let summary = merge_peer_states(&members).expect("summary");
        assert_eq!(
            summary.active_addr.as_deref(),
            Some("[2804:2b24:0:6::1]:4242")
        );
        assert_eq!(summary.rtt_ms, 9);
        assert!(!summary.alive);
    }

    #[test]
    fn merge_without_members_is_none() {
        assert_eq!(merge_peer_states(&[]), None);
    }
}
