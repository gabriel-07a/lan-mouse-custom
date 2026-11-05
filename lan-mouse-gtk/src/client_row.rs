mod imp;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::{self, Object};

use lan_mouse_ipc::{DEFAULT_PORT, Position};

use super::ClientObject;

/// Connection state of one client row.
///
/// Derived exclusively from properties the service publishes on
/// [`ClientObject`] (`active`, `alive`, `active-addr`, `incoming-addr`): the
/// row never invents a connection that is not backed by one of them.
///
/// [`Self::Connected`], [`Self::ConnectedBidirectional`],
/// [`Self::ConnectedDegraded`] and [`Self::Receiving`] are backed by an
/// established link (a completed DTLS handshake or an accepted incoming
/// session). [`Self::Connecting`] is only an attempt, [`Self::Disabled`] is
/// off. There is deliberately no "Reconnecting" state: a link that exists is
/// not being re-established, and `alive` is not a liveness signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowConnectionState {
    /// Turned off locally and no peer is connected to us.
    Disabled,
    /// Enabled, but no link is up in either direction yet.
    Connecting,
    /// Outgoing link established, but the peer reports its input emulation as
    /// disabled (`alive`: set optimistically at the handshake, refreshed by
    /// Pongs). The link itself is up, so this is still "Connected", only
    /// degraded.
    ConnectedDegraded,
    /// Outgoing link established and the peer reports input emulation enabled.
    Connected,
    /// Both an outgoing and an incoming link are established.
    ConnectedBidirectional,
    /// The peer holds an accepted incoming session to us (it can control
    /// this machine while it is entered); the outgoing direction may still be
    /// connecting.
    Receiving,
}

impl RowConnectionState {
    /// Label of the status pill. Only states backed by an established link may
    /// claim a connection.
    fn badge_label(self) -> &'static str {
        match self {
            Self::Disabled => "Disconnected",
            Self::Connecting => "Connecting…",
            Self::Connected | Self::ConnectedBidirectional | Self::ConnectedDegraded => "Connected",
            // The incoming session is accepted, but the service has no
            // evidence that the peer entered this screen (or sends input), so
            // this deliberately does not say plain "Connected" nor "Receiving
            // input".
            Self::Receiving => "Peer connected",
        }
    }

    /// CSS class of the status pill.
    fn badge_css_class(self) -> &'static str {
        match self {
            Self::Disabled => "status-offline",
            Self::Connecting => "status-connecting",
            // The link is up, but the peer cannot apply our input: amber.
            Self::ConnectedDegraded => "status-degraded",
            Self::Connected | Self::ConnectedBidirectional | Self::Receiving => "status-connected",
        }
    }

    /// Whether a link is still being established: shows the small spinner.
    ///
    /// `active` is the row's activation flag and only matters for
    /// [`Self::Receiving`]: the incoming session is already open, but with the
    /// outgoing direction enabled the connection attempt is still pending.
    /// A degraded link is up, so it does not spin.
    fn is_pending(self, active: bool) -> bool {
        match self {
            Self::Connecting => true,
            Self::Receiving => active,
            _ => false,
        }
    }
}

/// Every CSS class that can be applied to the status pill.
///
/// The cleanup loop in [`ClientRow::refresh_status`] must remove all of them
/// before applying the class of the new state, otherwise an old colour would
/// linger on the badge. Keep this in sync with
/// [`RowConnectionState::badge_css_class`]; the
/// `every_badge_class_is_cleared` test enforces it.
const STATUS_BADGE_CLASSES: [&str; 5] = [
    "status-connected",
    "status-connecting",
    "status-waiting",
    "status-offline",
    "status-degraded",
];

/// Derive the row state from the properties the service publishes.
///
/// `has_outgoing` means the service holds an established outgoing address for
/// this client (`active-addr`, written only after the DTLS handshake
/// completed); `has_incoming` means the peer holds an accepted incoming
/// session to this machine (`incoming-addr`, written on the DTLS accept).
///
/// **`alive` is not a liveness signal**: it is the peer's "input emulation
/// enabled" flag from its last Pong, and it is *also* set optimistically right
/// after the handshake. An established link whose peer reports emulation
/// disabled is [`RowConnectionState::ConnectedDegraded`], never a
/// reconnection.
pub(crate) fn row_connection_state(
    active: bool,
    alive: bool,
    has_outgoing: bool,
    has_incoming: bool,
) -> RowConnectionState {
    match (active, has_outgoing, alive, has_incoming) {
        // Turning the client off does not tear down a connection the peer
        // opened towards us, so that link still counts as connected.
        (false, _, _, false) => RowConnectionState::Disabled,
        (false, _, _, true) => RowConnectionState::Receiving,
        // An established outgoing link whose peer reports emulation enabled
        // is connected; both directions are only claimed when both exist.
        (true, true, true, true) => RowConnectionState::ConnectedBidirectional,
        (true, true, true, false) => RowConnectionState::Connected,
        // The link is up but the peer cannot apply our input: connected, not
        // reconnecting (and no spinner: nothing is being re-established).
        (true, true, false, _) => RowConnectionState::ConnectedDegraded,
        // Enabled with only the incoming direction up: an incoming session is
        // accepted even though the outgoing attempt has not succeeded yet.
        (true, false, _, true) => RowConnectionState::Receiving,
        (true, false, _, false) => RowConnectionState::Connecting,
    }
}

/// The single action a row offers for one state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    /// Turn the client on and try to connect to it.
    Connect,
    /// The peer is already connected to us; also enable the outgoing direction.
    ConnectAlso,
    /// Stop a pending connection attempt.
    Cancel,
    /// Turn the client off and drop the outgoing connection.
    Disconnect,
}

impl RowAction {
    fn label(self) -> &'static str {
        match self {
            Self::Connect => "Connect",
            Self::ConnectAlso => "Also connect",
            Self::Cancel => "Cancel",
            Self::Disconnect => "Disconnect",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::Connect => "Enable this device and try to connect to it",
            Self::ConnectAlso => {
                "An incoming session from the other PC is open — connect back to also control it"
            }
            // `deactivate_client` does not close the DTLS socket (and a
            // handshake already in flight is not re-validated), so these only
            // promise what the flag change actually does.
            Self::Cancel => "Stop trying to reach the other PC; an incoming session stays active",
            Self::Disconnect => "Disable this device",
        }
    }

    /// Whether pressing the button turns the client on.
    fn wants_activation(self) -> bool {
        matches!(self, Self::Connect | Self::ConnectAlso)
    }
}

/// Pick the single action for a state.
///
/// `active` disambiguates [`RowConnectionState::Receiving`]: when the outgoing
/// attempt is already enabled, "also connect" would be a no-op, so the honest
/// action is cancelling the attempt.
pub(crate) fn row_action(state: RowConnectionState, active: bool) -> RowAction {
    match state {
        RowConnectionState::Disabled => RowAction::Connect,
        RowConnectionState::Connecting => RowAction::Cancel,
        RowConnectionState::Connected
        | RowConnectionState::ConnectedBidirectional
        | RowConnectionState::ConnectedDegraded => RowAction::Disconnect,
        RowConnectionState::Receiving if active => RowAction::Cancel,
        RowConnectionState::Receiving => RowAction::ConnectAlso,
    }
}

/// Human-readable connection target for the `Connecting` subtitle:
/// `host:port` or `ip:port` (IPv6 literals are bracketed). `None` when the
/// row knows neither a hostname nor an IP yet.
fn connection_target(
    hostname: Option<&str>,
    primary_ip: Option<&str>,
    port: u32,
) -> Option<String> {
    let target = hostname.or(primary_ip)?;
    if port == 0 {
        return Some(target.to_string());
    }
    match target.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(_)) => Some(format!("[{target}]:{port}")),
        _ => Some(format!("{target}:{port}")),
    }
}

/// Tooltip for the status pill: explains what the pill means using only the
/// evidence the available fields provide, and never claims liveness.
fn row_tooltip(state: RowConnectionState, rtt: u32, active: bool) -> String {
    match state {
        RowConnectionState::Disabled => {
            "Not connected. Press Connect to enable this device and try to reach it.".to_string()
        }
        RowConnectionState::Connecting => "No connection yet. Check that the other PC is running and reachable; if it shows a pending request, approve it there."
            .to_string(),
        RowConnectionState::ConnectedDegraded => "The link to the other PC is established, but its input emulation is disabled. Enable emulation there to control it."
            .to_string(),
        RowConnectionState::Connected => {
            if rtt > 0 {
                format!("Connection established. Last round-trip time: {rtt} ms.")
            } else {
                "Connection established.".to_string()
            }
        }
        RowConnectionState::ConnectedBidirectional => {
            if rtt > 0 {
                format!(
                    "Connection established in both directions. Last round-trip time: {rtt} ms."
                )
            } else {
                "Connection established in both directions.".to_string()
            }
        }
        RowConnectionState::Receiving => {
            let base = "The other PC has an open session to this machine. Input is applied only while it enters your screen.";
            if active {
                format!("{base} The outgoing connection is still being established.")
            } else {
                base.to_string()
            }
        }
    }
}

/// Row state derived from the properties of one client object.
fn client_connection_state(client_object: &ClientObject) -> RowConnectionState {
    row_connection_state(
        client_object.property::<bool>("active"),
        client_object.property::<bool>("alive"),
        client_object
            .property::<Option<String>>("active-addr")
            .is_some(),
        client_object
            .property::<Option<String>>("incoming-addr")
            .is_some(),
    )
}

glib::wrapper! {
    pub struct ClientRow(ObjectSubclass<imp::ClientRow>)
    @extends gtk::ListBoxRow, gtk::Widget, adw::PreferencesRow, adw::ExpanderRow,
    @implements gtk::Accessible, gtk::Actionable, gtk::Buildable, gtk::ConstraintTarget;
}

impl ClientRow {
    pub fn new(client_object: &ClientObject) -> Self {
        let client_row: Self = Object::builder().build();
        client_row
            .imp()
            .client_object
            .borrow_mut()
            .replace(client_object.clone());
        client_row
    }

    pub fn bind(&self, client_object: &ClientObject) {
        let mut bindings = self.imp().bindings.borrow_mut();

        // bind hostname to hostname edit field
        let hostname_binding = client_object
            .bind_property("hostname", &self.imp().hostname.get(), "text")
            .transform_to(|_, v: Option<String>| {
                if let Some(hostname) = v {
                    Some(hostname)
                } else {
                    Some("".to_string())
                }
            })
            .sync_create()
            .build();

        // bind port to port edit field
        let port_binding = client_object
            .bind_property("port", &self.imp().port.get(), "text")
            .transform_to(|_, v: u32| {
                if v == DEFAULT_PORT as u32 {
                    Some("".to_string())
                } else {
                    Some(v.to_string())
                }
            })
            .sync_create()
            .build();

        // bind position to selected position
        let position_binding = client_object
            .bind_property("position", &self.imp().position.get(), "selected")
            .transform_to(|_, v: String| match v.as_str() {
                "right" => Some(1u32),
                "top" => Some(2u32),
                "bottom" => Some(3u32),
                _ => Some(0u32),
            })
            .sync_create()
            .build();

        // bind resolving status to spinner visibility
        let resolve_binding = client_object
            .bind_property(
                "resolving",
                &self.imp().dns_loading_indicator.get(),
                "spinning",
            )
            .sync_create()
            .build();

        // bind ips to tooltip-text
        let ip_binding = client_object
            .bind_property("ips", &self.imp().dns_button.get(), "tooltip-text")
            .transform_to(|_, ips: Vec<String>| {
                if ips.is_empty() {
                    Some("no ip addresses associated with this client".into())
                } else {
                    Some(ips.join("\n"))
                }
            })
            .sync_create()
            .build();

        bindings.push(hostname_binding);
        bindings.push(port_binding);
        bindings.push(position_binding);
        bindings.push(resolve_binding);
        bindings.push(ip_binding);

        // Render the initial collapsed title, badge, action and subtitle.
        // Subsequent changes are pushed by `Window::update_client_state` /
        // `update_client_config` calling `refresh_status` after writing the
        // new properties.
        self.refresh_status();
    }

    pub fn unbind(&self) {
        for binding in self.imp().bindings.borrow_mut().drain(..) {
            binding.unbind();
        }
    }

    pub fn set_hostname(&self, hostname: Option<String>) {
        self.imp().set_hostname(hostname);
    }

    pub fn set_port(&self, port: u16) {
        self.imp().set_port(port);
    }

    pub fn set_position(&self, pos: Position) {
        self.imp().set_pos(pos);
    }

    pub fn set_sensitivity(&self, sensitivity: u16) {
        self.imp().set_sensitivity(sensitivity);
    }

    pub fn set_dns_state(&self, resolved: bool) {
        self.imp().set_dns_state(resolved);
    }

    /// Recompute the collapsed title, status badge, spinner, single action
    /// button, badge tooltip and subtitle from the client object properties.
    ///
    /// Connection state is the primary information; a peer build mismatch is
    /// only appended as a warning suffix. The dns-status icon is left to its
    /// existing `set_dns_state` handler so the two indicators don't fight for
    /// the same CSS class.
    pub fn refresh_status(&self) {
        let data = {
            let client_object = self.imp().client_object.borrow();
            client_object.as_ref().map(|client_object| {
                (
                    client_connection_state(client_object),
                    client_object.property::<bool>("active"),
                    client_object.property::<Option<String>>("active-addr"),
                    client_object.property::<Option<String>>("incoming-addr"),
                    client_object.property::<u32>("port"),
                    client_object.property::<u32>("rtt-ms"),
                    client_object.property::<Option<String>>("peer-commit"),
                    client_object.property::<Option<String>>("hostname"),
                    client_object.property::<Vec<String>>("ips"),
                    client_object.property::<String>("position"),
                )
            })
        };
        let Some((state, active, addr, incoming_addr, port, rtt, peer, hostname, ips, position)) =
            data
        else {
            return;
        };

        // Compute row title: prefer hostname, fall back to first IP, then placeholder.
        let primary_ip = ips.first().cloned();
        let trimmed_hostname = hostname.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let title_markup = match (trimmed_hostname, primary_ip.as_deref()) {
            (Some(host), Some(ip)) if host != ip => {
                let h = glib::markup_escape_text(host);
                let ip_esc = glib::markup_escape_text(ip);
                format!("{h} <span alpha=\"65%\">({ip_esc})</span>")
            }
            (Some(host), _) => glib::markup_escape_text(host).to_string(),
            (None, Some(ip)) => glib::markup_escape_text(ip).to_string(),
            (None, None) => {
                "<span font_style=\"italic\" font_weight=\"light\" foreground=\"darkgrey\">Unnamed device (set IP or hostname)</span>".to_string()
            }
        };
        self.set_title(&title_markup);

        let local_commit = crate::local_commit_str();
        let local = glib::markup_escape_text(&local_commit).to_string();
        let rtt_suffix = if rtt > 0 {
            format!(" · {rtt} ms")
        } else {
            String::new()
        };
        let pos_label = match position.as_str() {
            "right" => "Right",
            "top" => "Top",
            "bottom" => "Bottom",
            _ => "Left",
        };
        let target = connection_target(trimmed_hostname, primary_ip.as_deref(), port)
            .map(|target| glib::markup_escape_text(&target).to_string());
        let addr = addr.map(|addr| glib::markup_escape_text(&addr).to_string());
        let incoming = incoming_addr.map(|addr| glib::markup_escape_text(&addr).to_string());

        self.remove_css_class("peer-mismatch");
        self.remove_css_class("peer-match");
        self.remove_css_class("peer-unknown");
        self.remove_css_class("client-offline");
        self.remove_css_class("client-waiting");
        self.remove_css_class("client-degraded");

        // Status pill: label + colour class, plus the pending spinner that is
        // only visible while a link is being established.
        let badge = &self.imp().status_badge;
        for class in STATUS_BADGE_CLASSES {
            badge.remove_css_class(class);
        }
        badge.set_label(state.badge_label());
        badge.add_css_class(state.badge_css_class());

        let spinner = &self.imp().connect_spinner;
        let pending = state.is_pending(active);
        spinner.set_visible(pending);
        spinner.set_spinning(pending);

        let subtitle = match state {
            RowConnectionState::Disabled => {
                self.add_css_class("client-offline");
                format!("Disconnected · Screen: {pos_label}")
            }
            RowConnectionState::Connecting => {
                self.add_css_class("client-waiting");
                // No evidence of *why* there is no link yet (peer off,
                // firewall, DNS, certificate or a pending approval), so only
                // state the observable fact: no answer yet.
                match target {
                    Some(target) => {
                        format!("Trying {target} · Screen: {pos_label} — no answer yet")
                    }
                    None => format!("Trying to connect · Screen: {pos_label} — no answer yet"),
                }
            }
            RowConnectionState::ConnectedDegraded => {
                self.add_css_class("client-degraded");
                let warning = "emulation disabled on the other PC";
                match addr {
                    Some(addr) => {
                        format!("Connected · {addr} · Screen: {pos_label}{rtt_suffix} — {warning}")
                    }
                    None => {
                        format!("Connected · Screen: {pos_label}{rtt_suffix} — {warning}")
                    }
                }
            }
            RowConnectionState::Connected => {
                self.add_css_class("peer-match");
                match addr {
                    Some(addr) => {
                        format!("Connected · {addr} · Screen: {pos_label}{rtt_suffix}")
                    }
                    None => format!("Connected · Screen: {pos_label}{rtt_suffix}"),
                }
            }
            RowConnectionState::ConnectedBidirectional => {
                self.add_css_class("peer-match");
                match addr {
                    Some(addr) => format!(
                        "Connected (bidirectional) · {addr} · Screen: {pos_label}{rtt_suffix}"
                    ),
                    None => {
                        format!("Connected (bidirectional) · Screen: {pos_label}{rtt_suffix}")
                    }
                }
            }
            RowConnectionState::Receiving => {
                self.add_css_class("peer-match");
                let from = incoming
                    .clone()
                    .unwrap_or_else(|| "the other PC".to_string());
                // `incoming-addr` proves only that the incoming DTLS session
                // was accepted: the peer may never enter this screen and the
                // service is not told when it leaves, so never claim input.
                if active {
                    format!(
                        "Incoming session from {from} · Screen: {pos_label} — outgoing connection still being established"
                    )
                } else {
                    format!("Incoming session from {from} · Screen: {pos_label}")
                }
            }
        };

        // Badge tooltip: explain what the pill actually means, never claim a
        // connection that does not exist.
        badge.set_tooltip_text(Some(&row_tooltip(state, rtt, active)));

        // Single action per state: "Connect", "Also connect", "Cancel" or
        // "Disconnect". Only the action that starts a connection is suggested.
        let action = row_action(state, active);
        let connect_btn = &self.imp().connect_button;
        connect_btn.set_label(action.label());
        connect_btn.set_tooltip_text(Some(action.tooltip()));
        if action == RowAction::Connect {
            connect_btn.add_css_class("suggested-action");
        } else {
            connect_btn.remove_css_class("suggested-action");
        }

        // Only mention the build when it actually differs: matching builds
        // are the normal case and the information is noise.
        let subtitle = match peer.as_deref() {
            Some(p) if p != local.as_str() => {
                self.add_css_class("peer-mismatch");
                let p = glib::markup_escape_text(p);
                format!("{subtitle} · peer build {p} (ours {local})")
            }
            _ => subtitle,
        };

        self.set_subtitle(&subtitle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(
        active: bool,
        alive: bool,
        has_outgoing: bool,
        has_incoming: bool,
    ) -> RowConnectionState {
        row_connection_state(active, alive, has_outgoing, has_incoming)
    }

    /// Every state, used to sweep badge/spinner invariants.
    const ALL_STATES: [RowConnectionState; 6] = [
        RowConnectionState::Disabled,
        RowConnectionState::Connecting,
        RowConnectionState::ConnectedDegraded,
        RowConnectionState::Connected,
        RowConnectionState::ConnectedBidirectional,
        RowConnectionState::Receiving,
    ];

    #[test]
    fn disabled_means_off_and_no_incoming_connection() {
        assert_eq!(
            state(false, false, false, false),
            RowConnectionState::Disabled
        );
        // Stale link/emulation data must not make an inactive row look connected.
        assert_eq!(
            state(false, true, true, false),
            RowConnectionState::Disabled
        );
    }

    #[test]
    fn incoming_connection_counts_even_when_local_activation_is_off() {
        assert_eq!(
            state(false, false, false, true),
            RowConnectionState::Receiving
        );
        assert_eq!(
            state(false, true, false, true),
            RowConnectionState::Receiving
        );
    }

    #[test]
    fn enabled_without_any_established_link_is_connecting() {
        assert_eq!(
            state(true, false, false, false),
            RowConnectionState::Connecting
        );
        // `alive` alone is not a link: without `active-addr` there is nothing
        // established yet, so the row must not claim to be connected.
        assert_eq!(
            state(true, true, false, false),
            RowConnectionState::Connecting
        );
    }

    #[test]
    fn established_outgoing_link_is_connected() {
        assert_eq!(
            state(true, true, true, false),
            RowConnectionState::Connected
        );
    }

    #[test]
    fn established_links_in_both_directions_are_bidirectional() {
        assert_eq!(
            state(true, true, true, true),
            RowConnectionState::ConnectedBidirectional
        );
    }

    #[test]
    fn link_with_peer_emulation_disabled_is_connected_degraded() {
        // `alive == false` means the peer's input emulation is off, not that
        // the link is gone: `active-addr` (the DTLS link) still exists.
        assert_eq!(
            state(true, false, true, false),
            RowConnectionState::ConnectedDegraded
        );
        // An open incoming session does not change that: the outgoing
        // direction exists but is unusable until emulation is enabled there.
        assert_eq!(
            state(true, false, true, true),
            RowConnectionState::ConnectedDegraded
        );
        // The badge never claims the link is being re-established.
        assert_eq!(
            RowConnectionState::ConnectedDegraded.badge_label(),
            "Connected"
        );
    }

    #[test]
    fn enabled_client_with_only_an_incoming_link_is_receiving() {
        assert_eq!(
            state(true, false, false, true),
            RowConnectionState::Receiving
        );
    }

    #[test]
    fn badge_only_claims_connected_for_real_links() {
        for state in ALL_STATES {
            let claims_connected = state.badge_label() == "Connected";
            assert_eq!(
                claims_connected,
                matches!(
                    state,
                    RowConnectionState::Connected
                        | RowConnectionState::ConnectedBidirectional
                        | RowConnectionState::ConnectedDegraded
                ),
                "{state:?} must only claim Connected for an established link"
            );
        }
    }

    #[test]
    fn every_badge_class_is_cleared_before_being_reapplied() {
        // Regression guard: a class missing from `STATUS_BADGE_CLASSES` would
        // stay on the badge after a state change (e.g. amber `status-degraded`
        // lingering after recovering to green `status-connected`).
        for state in ALL_STATES {
            let class = state.badge_css_class();
            assert!(
                STATUS_BADGE_CLASSES.contains(&class),
                "{state:?} uses status class {class:?}, which the cleanup loop does not remove"
            );
        }
    }

    #[test]
    fn badge_never_claims_reconnection_or_input_receipt() {
        for state in ALL_STATES {
            let label = state.badge_label();
            assert!(!label.contains("Reconnect"), "{state:?} label: {label}");
            assert!(!label.contains("Receiving"), "{state:?} label: {label}");
        }
        assert_eq!(
            RowConnectionState::Receiving.badge_label(),
            "Peer connected"
        );
    }

    #[test]
    fn badge_colours_match_the_connection_state() {
        assert_eq!(
            RowConnectionState::Disabled.badge_css_class(),
            "status-offline"
        );
        assert_eq!(
            RowConnectionState::Connecting.badge_css_class(),
            "status-connecting"
        );
        assert_eq!(
            RowConnectionState::ConnectedDegraded.badge_css_class(),
            "status-degraded"
        );
        for state in [
            RowConnectionState::Connected,
            RowConnectionState::ConnectedBidirectional,
            RowConnectionState::Receiving,
        ] {
            assert_eq!(state.badge_css_class(), "status-connected");
        }
    }

    #[test]
    fn spinner_only_turns_while_a_link_is_pending() {
        for state in ALL_STATES {
            let expected = matches!(
                state,
                RowConnectionState::Connecting | RowConnectionState::Receiving
            );
            assert_eq!(
                state.is_pending(true),
                expected,
                "{state:?}: pending spinner with the outgoing direction enabled"
            );
        }
        // `Receiving` only spins when the outgoing attempt is actually on.
        assert!(RowConnectionState::Connecting.is_pending(false));
        assert!(!RowConnectionState::Receiving.is_pending(false));
        for state in [
            RowConnectionState::Disabled,
            RowConnectionState::Connected,
            RowConnectionState::ConnectedBidirectional,
            RowConnectionState::ConnectedDegraded,
        ] {
            assert!(
                !state.is_pending(false),
                "{state:?} must not show the spinner"
            );
        }
    }

    #[test]
    fn exactly_one_action_per_state() {
        assert_eq!(
            row_action(RowConnectionState::Disabled, false),
            RowAction::Connect
        );
        assert_eq!(
            row_action(RowConnectionState::Connecting, true),
            RowAction::Cancel
        );
        for state in [
            RowConnectionState::Connected,
            RowConnectionState::ConnectedBidirectional,
            RowConnectionState::ConnectedDegraded,
        ] {
            assert_eq!(row_action(state, true), RowAction::Disconnect);
        }
        assert_eq!(
            row_action(RowConnectionState::Receiving, false),
            RowAction::ConnectAlso
        );
        // The outgoing attempt is already on: "also connect" would be a
        // no-op, so this variant cancels instead.
        assert_eq!(
            row_action(RowConnectionState::Receiving, true),
            RowAction::Cancel
        );
    }

    #[test]
    fn actions_map_to_the_right_activation_request() {
        assert!(RowAction::Connect.wants_activation());
        assert!(RowAction::ConnectAlso.wants_activation());
        assert!(!RowAction::Cancel.wants_activation());
        assert!(!RowAction::Disconnect.wants_activation());
    }

    #[test]
    fn tooltips_do_not_claim_liveness() {
        for state in ALL_STATES {
            for rtt in [0, 12] {
                let tooltip = row_tooltip(state, rtt, true);
                assert!(
                    !tooltip.to_lowercase().contains("live"),
                    "{state:?} tooltip claims liveness: {tooltip}"
                );
            }
        }
        assert_eq!(
            row_tooltip(RowConnectionState::Connected, 0, true),
            "Connection established."
        );
        assert_eq!(
            row_tooltip(RowConnectionState::Connected, 12, true),
            "Connection established. Last round-trip time: 12 ms."
        );
        assert_eq!(
            row_tooltip(RowConnectionState::ConnectedBidirectional, 0, true),
            "Connection established in both directions."
        );
        assert_eq!(
            row_tooltip(RowConnectionState::ConnectedBidirectional, 12, true),
            "Connection established in both directions. Last round-trip time: 12 ms."
        );
    }

    #[test]
    fn action_tooltips_do_not_promise_teardown() {
        // `deactivate_client` does not close the DTLS socket and an in-flight
        // handshake is not re-validated, so the action tooltips may only
        // describe the flag change itself.
        assert_eq!(RowAction::Disconnect.tooltip(), "Disable this device");
        assert_eq!(
            RowAction::Cancel.tooltip(),
            "Stop trying to reach the other PC; an incoming session stays active"
        );
        for action in [
            RowAction::Connect,
            RowAction::ConnectAlso,
            RowAction::Cancel,
            RowAction::Disconnect,
        ] {
            let tooltip = action.tooltip().to_lowercase();
            assert!(
                !tooltip.contains("disconnect"),
                "{action:?} tooltip promises a disconnect: {tooltip}"
            );
        }
    }

    #[test]
    fn connection_target_includes_the_port_and_brackets_ipv6() {
        assert_eq!(
            connection_target(Some("windows11"), Some("192.0.2.10"), 4242).as_deref(),
            Some("windows11:4242")
        );
        assert_eq!(
            connection_target(None, Some("192.0.2.10"), 4242).as_deref(),
            Some("192.0.2.10:4242")
        );
        assert_eq!(
            connection_target(None, Some("2001:db8::1"), 4242).as_deref(),
            Some("[2001:db8::1]:4242")
        );
        assert_eq!(connection_target(None, None, 4242), None);
        // A missing port is not rendered as ":0".
        assert_eq!(
            connection_target(Some("windows11"), None, 0).as_deref(),
            Some("windows11")
        );
    }

    #[test]
    fn state_is_derived_from_the_client_object_properties() {
        let client = glib::Object::builder::<ClientObject>()
            .property("active", true)
            .property("alive", true)
            .property("active-addr", Some("192.0.2.10:4242".to_string()))
            .build();
        assert_eq!(
            client_connection_state(&client),
            RowConnectionState::Connected
        );

        client.set_property("incoming-addr", Some("192.0.2.10:4242".to_string()));
        assert_eq!(
            client_connection_state(&client),
            RowConnectionState::ConnectedBidirectional
        );

        // `alive` is the peer emulation flag: turning it off degrades a live
        // link, it does not turn it into a reconnection.
        client.set_property("alive", false);
        assert_eq!(
            client_connection_state(&client),
            RowConnectionState::ConnectedDegraded
        );

        client.set_property("active", false);
        assert_eq!(
            client_connection_state(&client),
            RowConnectionState::Receiving
        );
    }
}
