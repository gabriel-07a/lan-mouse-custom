mod imp;

use std::time::{Duration, Instant};

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::{self, Object};

use lan_mouse_ipc::{DEFAULT_PORT, Position};

use super::ClientObject;

/// Evidence that the current outgoing link is actually working.
///
/// `active-addr` only proves that the DTLS handshake completed; it says nothing
/// about the peer still answering. The only direct evidence of a working
/// round-trip is a recent `Pong`: the service exposes that as
/// [`lan_mouse_ipc::ClientState::last_pong_at`] when available, otherwise the
/// GUI falls back to the cadence of `FrontendEvent::State` updates (the service
/// broadcasts one per `Pong`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkProof {
    /// No round-trip observed on the current link yet. A handshake may have
    /// completed, but the row must not claim "Connected".
    Pending,
    /// The last observed round-trip was this many seconds ago and still counts
    /// as fresh.
    Recent { age_secs: u64 },
    /// The link answered before, but the last reply is this many seconds old:
    /// the peer stopped answering (asleep, offline, firewall, cable pulled).
    Stale { age_secs: u64 },
}

impl LinkProof {
    /// Whether the peer answered recently enough to call the link connected.
    fn is_fresh(self) -> bool {
        matches!(self, Self::Recent { .. })
    }

    fn age_secs(self) -> Option<u64> {
        match self {
            Self::Pending => None,
            Self::Recent { age_secs } | Self::Stale { age_secs } => Some(age_secs),
        }
    }

    /// Combine the proofs of two entries that belong to the same logical peer
    /// (duplicate configured clients for one identity): a fresh reply from any
    /// entry proves the link; otherwise report the most recent stale age.
    pub(crate) fn best(self, other: Self) -> Self {
        match (self, other) {
            (Self::Recent { age_secs: a }, Self::Recent { age_secs: b }) => {
                Self::Recent { age_secs: a.min(b) }
            }
            (recent @ Self::Recent { .. }, _) | (_, recent @ Self::Recent { .. }) => recent,
            (Self::Stale { age_secs: a }, Self::Stale { age_secs: b }) => {
                Self::Stale { age_secs: a.min(b) }
            }
            (stale @ Self::Stale { .. }, _) | (_, stale @ Self::Stale { .. }) => stale,
            _ => Self::Pending,
        }
    }
}

/// Derive a [`LinkProof`] from the age of the last observed round-trip.
///
/// `fresh` is the window in which a reply still proves the link: the service
/// pings in rounds of ~2 s and closes a link that stopped answering after two
/// missed rounds, so 6 s is conservative (it also absorbs scheduling jitter
/// without flickering the badge on every missed packet).
pub fn link_proof_from_age(age: std::time::Duration, fresh: std::time::Duration) -> LinkProof {
    let age_secs = age.as_secs();
    if age <= fresh {
        LinkProof::Recent { age_secs }
    } else {
        LinkProof::Stale { age_secs }
    }
}

/// Connection state of one client row.
///
/// Derived from the properties the service publishes on [`ClientObject`]
/// (`active`, `alive`, `active-addr`, `incoming-addr`) **plus** the recent
/// round-trip evidence ([`LinkProof`]). An established link alone is not
/// enough: `active-addr` is written right after the DTLS handshake, and `alive`
/// is the peer's "input emulation enabled" flag (set optimistically at the
/// handshake), so neither proves the peer still answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowConnectionState {
    /// Turned off locally and no peer is connected to us.
    Disabled,
    /// Enabled, but no round-trip is proven yet (includes a completed
    /// handshake that never got an answer).
    Connecting,
    /// The link answered before but the peer went silent: the row must say so
    /// instead of pretending to be connected.
    Reconnecting,
    /// Outgoing link established and answering, but the peer reports its input
    /// emulation as disabled. The link itself is up, so this is still
    /// "Connected", only degraded.
    ConnectedDegraded,
    /// Outgoing link established and answering, peer emulation enabled.
    Connected,
    /// Both an outgoing and an incoming link are established and the outgoing
    /// one is answering.
    ConnectedBidirectional,
    /// The peer holds an accepted incoming session to us (it can control this
    /// machine while it is entered); the outgoing direction may still be
    /// missing or unconfirmed.
    Receiving,
}

impl RowConnectionState {
    /// Label of the status pill. Only states backed by a recent round-trip may
    /// claim a connection.
    fn badge_label(self) -> &'static str {
        match self {
            Self::Disabled => "Disconnected",
            Self::Connecting => "Connecting…",
            Self::Reconnecting => "Reconnecting…",
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
            // Both "trying" and "was working, now silent" are amber: never
            // green while the peer does not answer.
            Self::Reconnecting => "status-degraded",
            // The link is up, but the peer cannot apply our input: amber.
            Self::ConnectedDegraded => "status-degraded",
            Self::Connected | Self::ConnectedBidirectional | Self::Receiving => "status-connected",
        }
    }

    /// Whether the row is waiting on something: shows the small spinner.
    ///
    /// `active` disambiguates [`Self::Receiving`]: the incoming session is
    /// already open, but with the outgoing direction enabled the attempt is
    /// still pending. A degraded link is up, so it does not spin.
    fn is_pending(self, active: bool) -> bool {
        match self {
            Self::Connecting | Self::Reconnecting => true,
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

/// Derive the row state from the properties the service publishes plus the
/// round-trip proof.
///
/// `has_outgoing` means the service holds an established outgoing address for
/// this client (`active-addr`, written after the DTLS handshake completed);
/// `has_incoming` means the peer holds an accepted incoming session to this
/// machine (`incoming-addr`, written on the DTLS accept).
///
/// **`alive` is not a liveness signal**: it is the peer's "input emulation
/// enabled" flag from its last Pong, and it is *also* set optimistically right
/// after the handshake. `alive=true` with a stale proof is a dead link, and
/// `alive=false` with a fresh proof is a working link whose emulation is off.
pub(crate) fn row_connection_state(
    active: bool,
    alive: bool,
    has_outgoing: bool,
    has_incoming: bool,
    proof: LinkProof,
) -> RowConnectionState {
    if !active {
        return if has_incoming {
            RowConnectionState::Receiving
        } else {
            RowConnectionState::Disabled
        };
    }
    if !has_outgoing {
        return if has_incoming {
            RowConnectionState::Receiving
        } else {
            RowConnectionState::Connecting
        };
    }
    // An outgoing link exists, but "connected" additionally requires a recent
    // round-trip: a handshake alone is not proof that input will be delivered.
    if !proof.is_fresh() {
        return match proof {
            LinkProof::Stale { .. } => RowConnectionState::Reconnecting,
            LinkProof::Pending | LinkProof::Recent { .. } => RowConnectionState::Connecting,
        };
    }
    match (alive, has_incoming) {
        (true, true) => RowConnectionState::ConnectedBidirectional,
        (true, false) => RowConnectionState::Connected,
        (false, _) => RowConnectionState::ConnectedDegraded,
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
        | RowConnectionState::ConnectedDegraded
        | RowConnectionState::Reconnecting => RowAction::Disconnect,
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

/// Parse the numeric IP out of either a bare address or an `ip:port` /
/// `[ipv6]:port` endpoint.
fn parse_address_ip(address: &str) -> Option<std::net::IpAddr> {
    let address = address.trim();
    if let Ok(ip) = address.parse::<std::net::IpAddr>() {
        return Some(ip);
    }
    address
        .parse::<std::net::SocketAddr>()
        .ok()
        .map(|endpoint| endpoint.ip())
}

/// Human readable address family for an endpoint (or a bare IP): `IPv4`,
/// `IPv6`, or `None` when the text is neither (e.g. a hostname).
pub fn address_family_label(address: &str) -> Option<&'static str> {
    let ip = parse_address_ip(address)?;
    Some(if ip.is_ipv4() { "IPv4" } else { "IPv6" })
}

/// Render an endpoint with its family, e.g. `192.168.1.5:4242 (IPv4)` or
/// `[2804:…::1]:4242 (IPv6)`. The raw text is kept when it is not an IP so a
/// hostname is never mangled.
fn format_endpoint(address: &str) -> String {
    match address_family_label(address) {
        Some(family) => format!("{address} ({family})"),
        None => address.to_string(),
    }
}

/// Stable ordering for the address list: IPv4 first, then IPv6, numerically
/// sorted. The service transports `ClientState::ips` as a `HashSet` (arbitrary
/// iteration order), so the UI must never display the raw order or the
/// hostname/address would appear to alternate between updates.
pub fn sorted_ips(ips: &[String]) -> Vec<String> {
    let mut parsed = ips
        .iter()
        .filter_map(|ip| ip.trim().parse::<std::net::IpAddr>().ok())
        .collect::<Vec<_>>();
    parsed.sort();
    parsed.dedup();
    parsed.into_iter().map(|ip| ip.to_string()).collect()
}

/// Preferred address for the row: the address of the established outgoing
/// link, then the accepted incoming session, and finally the first entry of the
/// already sorted IP list. Deterministic for a given state.
fn preferred_address<'a>(
    active_addr: Option<&'a str>,
    incoming_addr: Option<&'a str>,
    ips: &'a [String],
) -> Option<&'a str> {
    active_addr
        .or(incoming_addr)
        .or_else(|| ips.first().map(String::as_str))
}

/// Debounce a flapping endpoint so the subtitle does not alternate between
/// IPv4 and IPv6 on every packet.
///
/// `shown`/`candidate` are caller-owned memory (one pair per peer). A new
/// endpoint replaces the shown one immediately when nothing was shown yet, and
/// only after it has persisted for `hold` otherwise. Returns the endpoint that
/// should be displayed *now*.
pub fn sticky_endpoint(
    shown: &mut Option<String>,
    candidate: &mut Option<(String, Instant)>,
    raw: Option<String>,
    now: Instant,
    hold: Duration,
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

/// Tooltip for the status pill: explains what the pill means using only the
/// evidence the available fields provide, and never claims liveness.
fn row_tooltip(state: RowConnectionState, rtt: u32, active: bool, proof: LinkProof) -> String {
    match state {
        RowConnectionState::Disabled => {
            "Not connected. Press Connect to enable this device and try to reach it.".to_string()
        }
        RowConnectionState::Connecting => "No reply from the other PC yet. Check that it is running and reachable; if it shows a pending request, approve it there.".to_string(),
        RowConnectionState::Reconnecting => {
            let age = proof.age_secs().unwrap_or_default();
            format!(
                "The link worked, but the other PC has not answered for {age}s. It may be \
                 asleep, offline, or blocked by a firewall; the GUI will show it connected \
                 again as soon as a round-trip is observed."
            )
        }
        RowConnectionState::ConnectedDegraded => "The link is established and answering, but the other PC reports its input emulation as disabled. Enable emulation there to control it."
            .to_string(),
        RowConnectionState::Connected => {
            if rtt > 0 {
                format!("Round-trip confirmed. Last round-trip time: {rtt} ms.")
            } else {
                "Round-trip confirmed.".to_string()
            }
        }
        RowConnectionState::ConnectedBidirectional => {
            if rtt > 0 {
                format!(
                    "Round-trip confirmed in both directions. Last round-trip time: {rtt} ms."
                )
            } else {
                "Round-trip confirmed in both directions.".to_string()
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

/// Row state derived from the properties of one client object, using the
/// conservative "no round-trip evidence yet" proof.
///
/// This is the entry point used by the click handler (`client_row/imp.rs`) to
/// pick the action for the currently displayed row. Every state that can be
/// displayed with a fresh proof maps to the same activation bit as its
/// `Pending` fallback (Connected/Reconnecting/Degraded => deactivate,
/// Connecting => deactivate, Disabled => activate), so the click still matches
/// the button the user sees.
fn client_connection_state(client_object: &ClientObject) -> RowConnectionState {
    client_connection_state_with_proof(client_object, LinkProof::Pending)
}

/// Row state derived from the properties of one client object plus the
/// recent round-trip evidence.
fn client_connection_state_with_proof(
    client_object: &ClientObject,
    proof: LinkProof,
) -> RowConnectionState {
    row_connection_state(
        client_object.property::<bool>("active"),
        client_object.property::<bool>("alive"),
        client_object
            .property::<Option<String>>("active-addr")
            .is_some(),
        client_object
            .property::<Option<String>>("incoming-addr")
            .is_some(),
        proof,
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
    /// button, tooltip and subtitle from the client object properties plus the
    /// recent round-trip evidence.
    ///
    /// `proof` is the freshness of the last reply from this peer. Callers that
    /// have no freshness information yet pass [`LinkProof::Pending`], which can
    /// never display "Connected".
    pub fn refresh_status_with_proof(&self, proof: LinkProof) {
        let data = {
            let client_object = self.imp().client_object.borrow();
            client_object.as_ref().map(|client_object| {
                (
                    client_connection_state_with_proof(client_object, proof),
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

        // Stable address ordering: the service transports the address set as a
        // HashSet, so the raw order changes between updates. Sorting keeps the
        // title/tooltip from alternating between IPv4 and IPv6.
        let sorted = sorted_ips(&ips);
        let trimmed_hostname = hostname.as_deref().map(str::trim).filter(|s| !s.is_empty());
        // The preferred address is: the established outgoing endpoint, then the
        // accepted incoming one, then the first sorted IP. It never comes from
        // the unstable HashSet order.
        let preferred = preferred_address(addr.as_deref(), incoming_addr.as_deref(), &sorted)
            .map(str::to_string);

        // Title: hostname when known; otherwise the stable preferred address.
        // The address is not appended to the title anymore: the subtitle shows
        // the endpoint that is actually in use (with its family), so the title
        // cannot flap between address families.
        let title_markup = match (trimmed_hostname, preferred.as_deref()) {
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
        // A target for the "trying to reach it" states: hostname when known,
        // else the stable preferred IP (never the raw set order).
        let target = connection_target(trimmed_hostname, preferred.as_deref(), port)
            .map(|target| glib::markup_escape_text(&target).to_string());
        let endpoint = addr
            .as_deref()
            .map(|addr| glib::markup_escape_text(&format_endpoint(addr)).to_string());
        let incoming = incoming_addr
            .as_deref()
            .map(|addr| glib::markup_escape_text(&format_endpoint(addr)).to_string());
        let has_outgoing = addr.is_some();

        self.remove_css_class("peer-mismatch");
        self.remove_css_class("peer-match");
        self.remove_css_class("peer-unknown");
        self.remove_css_class("client-offline");
        self.remove_css_class("client-waiting");
        self.remove_css_class("client-degraded");

        // Status pill: label + colour class, plus the pending spinner.
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
                if has_outgoing {
                    // The handshake completed but no round-trip was observed:
                    // never call that "Connected".
                    match endpoint.or(target) {
                        Some(target) => {
                            format!(
                                "Link to {target} up · Screen: {pos_label} — waiting for a reply"
                            )
                        }
                        None => {
                            format!("Link established · Screen: {pos_label} — waiting for a reply")
                        }
                    }
                } else {
                    match target {
                        Some(target) => {
                            format!("Trying {target} · Screen: {pos_label} — no answer yet")
                        }
                        None => format!("Trying to connect · Screen: {pos_label} — no answer yet"),
                    }
                }
            }
            RowConnectionState::Reconnecting => {
                self.add_css_class("client-degraded");
                let age = proof.age_secs().unwrap_or_default();
                format!("No reply for {age}s — reconnecting… · Screen: {pos_label}")
            }
            RowConnectionState::ConnectedDegraded => {
                self.add_css_class("client-degraded");
                let warning = "emulation disabled on the other PC";
                match endpoint {
                    Some(addr) => {
                        format!("Connected · {addr} · Screen: {pos_label}{rtt_suffix} — {warning}")
                    }
                    None => format!("Connected · Screen: {pos_label}{rtt_suffix} — {warning}"),
                }
            }
            RowConnectionState::Connected => {
                self.add_css_class("peer-match");
                match endpoint {
                    Some(addr) => {
                        format!("Connected · {addr} · Screen: {pos_label}{rtt_suffix}")
                    }
                    None => format!("Connected · Screen: {pos_label}{rtt_suffix}"),
                }
            }
            RowConnectionState::ConnectedBidirectional => {
                self.add_css_class("peer-match");
                match endpoint {
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
                    .or_else(|| endpoint.clone())
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

        // Badge tooltip: explain what the pill actually means, using only the
        // evidence available, and never claim liveness without a round-trip.
        badge.set_tooltip_text(Some(&row_tooltip(state, rtt, active, proof)));

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

    /// Legacy entry point for callers without freshness information. A row that
    /// never receives a [`LinkProof`] stays in `Connecting` and can never show
    /// a false "Connected"; normal callers use
    /// [`Self::refresh_status_with_proof`].
    pub fn refresh_status(&self) {
        self.refresh_status_with_proof(LinkProof::Pending);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRESH: LinkProof = LinkProof::Recent { age_secs: 1 };

    fn state(
        active: bool,
        alive: bool,
        has_outgoing: bool,
        has_incoming: bool,
        proof: LinkProof,
    ) -> RowConnectionState {
        row_connection_state(active, alive, has_outgoing, has_incoming, proof)
    }

    fn connected_state(
        active: bool,
        alive: bool,
        has_outgoing: bool,
        has_incoming: bool,
    ) -> RowConnectionState {
        state(active, alive, has_outgoing, has_incoming, FRESH)
    }

    /// Every state, used to sweep badge/spinner invariants.
    const ALL_STATES: [RowConnectionState; 7] = [
        RowConnectionState::Disabled,
        RowConnectionState::Connecting,
        RowConnectionState::Reconnecting,
        RowConnectionState::ConnectedDegraded,
        RowConnectionState::Connected,
        RowConnectionState::ConnectedBidirectional,
        RowConnectionState::Receiving,
    ];

    #[test]
    fn disabled_means_off_and_no_incoming_connection() {
        assert_eq!(
            connected_state(false, false, false, false),
            RowConnectionState::Disabled
        );
        // Stale link/emulation data must not make an inactive row look connected.
        assert_eq!(
            connected_state(false, true, true, false),
            RowConnectionState::Disabled
        );
    }

    /// Regression for the user report: a completed handshake is not proof that
    /// input works. Without a round-trip the row must not say "Connected".
    #[test]
    fn handshake_without_round_trip_is_not_connected() {
        assert_eq!(
            state(true, true, true, false, LinkProof::Pending),
            RowConnectionState::Connecting
        );
        assert_eq!(
            state(true, true, true, true, LinkProof::Pending),
            RowConnectionState::Connecting
        );
        assert_ne!(
            state(true, true, true, false, LinkProof::Pending).badge_label(),
            "Connected"
        );
        // Never claims "Connected" from an optimistic `alive` plus a handshake.
        assert_eq!(
            state(true, true, true, false, LinkProof::Pending).badge_label(),
            "Connecting…"
        );
    }

    /// Regression for the user report: `alive` is set optimistically at the
    /// handshake and can be stale; only a recent round-trip proves the link.
    #[test]
    fn stale_proof_wins_over_optimistic_alive() {
        assert_eq!(
            state(true, true, true, false, LinkProof::Stale { age_secs: 7 }),
            RowConnectionState::Reconnecting
        );
        assert_eq!(
            state(true, true, true, true, LinkProof::Stale { age_secs: 7 }),
            RowConnectionState::Reconnecting
        );
        assert_ne!(
            state(true, true, true, false, LinkProof::Stale { age_secs: 7 }).badge_label(),
            "Connected"
        );
    }

    #[test]
    fn fresh_round_trip_makes_outgoing_link_connected() {
        assert_eq!(
            connected_state(true, true, true, false),
            RowConnectionState::Connected
        );
        assert_eq!(
            connected_state(true, true, true, true),
            RowConnectionState::ConnectedBidirectional
        );
        assert_eq!(
            connected_state(true, true, true, false).badge_label(),
            "Connected"
        );
    }

    #[test]
    fn fresh_link_with_peer_emulation_disabled_is_connected_degraded() {
        // `alive == false` means the peer's input emulation is off, not that
        // the link is gone: a recent round-trip proves the link is up.
        assert_eq!(
            connected_state(true, false, true, false),
            RowConnectionState::ConnectedDegraded
        );
        assert_eq!(
            connected_state(true, false, true, true),
            RowConnectionState::ConnectedDegraded
        );
        // The badge says "Connected" (the link is real) but the subtitle and
        // tooltip explain the degradation; it is never a reconnection.
        assert_eq!(
            RowConnectionState::ConnectedDegraded.badge_label(),
            "Connected"
        );
        assert_eq!(
            RowConnectionState::ConnectedDegraded.badge_css_class(),
            "status-degraded"
        );
        assert!(!RowConnectionState::ConnectedDegraded.is_pending(true));
    }

    #[test]
    fn receiving_is_unchanged_by_link_proof() {
        assert_eq!(
            state(false, false, false, true, LinkProof::Pending),
            RowConnectionState::Receiving
        );
        assert_eq!(
            state(false, true, false, true, LinkProof::Stale { age_secs: 30 }),
            RowConnectionState::Receiving
        );
        assert_eq!(
            state(true, false, false, true, LinkProof::Pending),
            RowConnectionState::Receiving
        );
        assert_eq!(
            RowConnectionState::Receiving.badge_label(),
            "Peer connected"
        );
    }

    #[test]
    fn badge_only_claims_connected_with_a_fresh_outgoing_round_trip() {
        let proofs = [
            LinkProof::Pending,
            LinkProof::Recent { age_secs: 1 },
            LinkProof::Stale { age_secs: 9 },
        ];
        for active in [false, true] {
            for alive in [false, true] {
                for has_outgoing in [false, true] {
                    for has_incoming in [false, true] {
                        for proof in proofs {
                            let state = state(active, alive, has_outgoing, has_incoming, proof);
                            let claims_connected = state.badge_label() == "Connected";
                            assert_eq!(
                                claims_connected,
                                active && has_outgoing && matches!(proof, LinkProof::Recent { .. }),
                                "{state:?} with {proof:?} (active={active}, alive={alive}, \
                                 out={has_outgoing}, in={has_incoming}) must not claim Connected"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn every_badge_class_is_cleared_before_being_reapplied() {
        for state in ALL_STATES {
            let class = state.badge_css_class();
            assert!(
                STATUS_BADGE_CLASSES.contains(&class),
                "{state:?} uses status class {class:?}, which the cleanup loop does not remove"
            );
        }
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
            RowConnectionState::Reconnecting.badge_css_class(),
            "status-degraded"
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
    fn spinner_turns_while_connecting_or_reconnecting() {
        assert!(RowConnectionState::Connecting.is_pending(false));
        assert!(RowConnectionState::Reconnecting.is_pending(false));
        assert!(!RowConnectionState::Connected.is_pending(false));
        assert!(!RowConnectionState::ConnectedBidirectional.is_pending(false));
        assert!(!RowConnectionState::ConnectedDegraded.is_pending(true));
        assert!(!RowConnectionState::Disabled.is_pending(true));
        // Receiving with the outgoing attempt enabled still shows the spinner.
        assert!(RowConnectionState::Receiving.is_pending(true));
        assert!(!RowConnectionState::Receiving.is_pending(false));
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
        // A link that stopped answering is still a configured link: the honest
        // action is to disable it, not to "cancel" an attempt that is not
        // pending anymore.
        assert_eq!(
            row_action(RowConnectionState::Reconnecting, true),
            RowAction::Disconnect
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
    fn link_proof_from_age_uses_the_fresh_window() {
        let fresh = Duration::from_secs(6);
        assert_eq!(
            link_proof_from_age(Duration::from_millis(400), fresh),
            LinkProof::Recent { age_secs: 0 }
        );
        assert_eq!(
            link_proof_from_age(Duration::from_secs(6), fresh),
            LinkProof::Recent { age_secs: 6 }
        );
        assert_eq!(
            link_proof_from_age(Duration::from_secs(7), fresh),
            LinkProof::Stale { age_secs: 7 }
        );
    }

    #[test]
    fn combined_proof_uses_the_best_member() {
        assert_eq!(
            LinkProof::Pending.best(LinkProof::Recent { age_secs: 2 }),
            LinkProof::Recent { age_secs: 2 }
        );
        assert_eq!(
            LinkProof::Recent { age_secs: 2 }.best(LinkProof::Recent { age_secs: 1 }),
            LinkProof::Recent { age_secs: 1 }
        );
        assert_eq!(
            LinkProof::Stale { age_secs: 9 }.best(LinkProof::Recent { age_secs: 1 }),
            LinkProof::Recent { age_secs: 1 }
        );
        assert_eq!(
            LinkProof::Stale { age_secs: 9 }.best(LinkProof::Stale { age_secs: 4 }),
            LinkProof::Stale { age_secs: 4 }
        );
        assert_eq!(
            LinkProof::Pending.best(LinkProof::Pending),
            LinkProof::Pending
        );
    }

    #[test]
    fn address_family_is_derived_from_the_endpoint() {
        assert_eq!(address_family_label("192.168.1.5"), Some("IPv4"));
        assert_eq!(address_family_label("192.168.1.5:4242"), Some("IPv4"));
        assert_eq!(address_family_label("2804:2b24::1"), Some("IPv6"));
        assert_eq!(address_family_label("[2804:2b24::1]:4242"), Some("IPv6"));
        assert_eq!(address_family_label("predator"), None);
        assert_eq!(address_family_label("predator:4242"), None);
    }

    #[test]
    fn endpoints_show_the_family() {
        assert_eq!(
            format_endpoint("192.168.1.5:4242"),
            "192.168.1.5:4242 (IPv4)"
        );
        assert_eq!(
            format_endpoint("[2804:2b24::1]:4242"),
            "[2804:2b24::1]:4242 (IPv6)"
        );
        // Hostnames must never be decorated with a bogus family.
        assert_eq!(format_endpoint("predator:4242"), "predator:4242");
    }

    #[test]
    fn sorted_ips_is_deterministic_with_ipv4_first() {
        let ips = vec![
            "2804:2b24::1".to_string(),
            "192.168.166.197".to_string(),
            "10.0.0.2".to_string(),
            "2804:2b24::1".to_string(),
        ];
        assert_eq!(
            sorted_ips(&ips),
            vec![
                "10.0.0.2".to_string(),
                "192.168.166.197".to_string(),
                "2804:2b24::1".to_string()
            ]
        );
        // Same input set in any order yields the same display list.
        let mut reversed = ips.clone();
        reversed.reverse();
        assert_eq!(sorted_ips(&ips), sorted_ips(&reversed));
        // Invalid entries are dropped instead of appearing in the row.
        assert_eq!(
            sorted_ips(&["not-an-ip".to_string(), "10.0.0.1".to_string()]),
            vec!["10.0.0.1".to_string()]
        );
    }

    #[test]
    fn preferred_address_is_stable_and_uses_the_established_link() {
        let ips = sorted_ips(&["2804:2b24::1".to_string(), "192.168.166.197".to_string()]);
        assert_eq!(
            preferred_address(Some("192.168.166.197:4242"), None, &ips),
            Some("192.168.166.197:4242")
        );
        assert_eq!(
            preferred_address(None, Some("[2804:2b24::1]:5000"), &ips),
            Some("[2804:2b24::1]:5000")
        );
        assert_eq!(preferred_address(None, None, &ips), Some("192.168.166.197"));
        assert_eq!(preferred_address(None, None, &[]), None);
    }

    #[test]
    fn sticky_endpoint_does_not_flap_between_families() {
        let hold = Duration::from_secs(3);
        let mut shown = None;
        let mut candidate = None;
        let start = Instant::now();

        // First address is shown immediately.
        assert_eq!(
            sticky_endpoint(
                &mut shown,
                &mut candidate,
                Some("192.168.166.197:4242".to_string()),
                start,
                hold
            ),
            Some("192.168.166.197:4242".to_string())
        );
        // A different address from the next packet does not replace it yet.
        let ipv6 = Some("[2804:2b24::1]:4242".to_string());
        for offset in [0u64, 1, 2] {
            assert_eq!(
                sticky_endpoint(
                    &mut shown,
                    &mut candidate,
                    ipv6.clone(),
                    start + Duration::from_secs(offset),
                    hold
                ),
                Some("192.168.166.197:4242".to_string()),
                "endpoint must stay stable within the hold window"
            );
        }
        // Once the new address persisted for the hold window it is shown.
        assert_eq!(
            sticky_endpoint(
                &mut shown,
                &mut candidate,
                ipv6.clone(),
                start + Duration::from_secs(3),
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
            client_connection_state_with_proof(&client, FRESH),
            RowConnectionState::Connected
        );
        // Without a recent round-trip the same properties are not enough.
        assert_eq!(
            client_connection_state_with_proof(&client, LinkProof::Pending),
            RowConnectionState::Connecting
        );

        client.set_property("incoming-addr", Some("192.0.2.10:4242".to_string()));
        assert_eq!(
            client_connection_state_with_proof(&client, FRESH),
            RowConnectionState::ConnectedBidirectional
        );

        // `alive` is the peer emulation flag: turning it off degrades a live
        // link, it does not turn it into a reconnection.
        client.set_property("alive", false);
        assert_eq!(
            client_connection_state_with_proof(&client, FRESH),
            RowConnectionState::ConnectedDegraded
        );

        // A stale proof always wins: the row must not claim Connected.
        assert_eq!(
            client_connection_state_with_proof(&client, LinkProof::Stale { age_secs: 12 }),
            RowConnectionState::Reconnecting
        );

        client.set_property("active", false);
        assert_eq!(
            client_connection_state_with_proof(&client, FRESH),
            RowConnectionState::Receiving
        );
    }
}
