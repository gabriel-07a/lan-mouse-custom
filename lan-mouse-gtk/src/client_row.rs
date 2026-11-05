mod imp;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::{self, Object};

use lan_mouse_ipc::{DEFAULT_PORT, Position};

use super::ClientObject;

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

        // bind client active to switch state
        let active_binding = client_object
            .bind_property("active", &self.imp().enable_switch.get(), "state")
            .sync_create()
            .build();

        // bind client active to switch position
        let switch_position_binding = client_object
            .bind_property("active", &self.imp().enable_switch.get(), "active")
            .sync_create()
            .build();

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

        bindings.push(active_binding);
        bindings.push(switch_position_binding);
        bindings.push(hostname_binding);
        bindings.push(port_binding);
        bindings.push(position_binding);
        bindings.push(resolve_binding);
        bindings.push(ip_binding);

        // Render the initial collapsed title, badge, and subtitle. Subsequent
        // changes are pushed by `Window::update_client_state` / `update_client_config`
        // calling `refresh_status` after writing the new properties.
        self.refresh_status();
    }

    pub fn unbind(&self) {
        for binding in self.imp().bindings.borrow_mut().drain(..) {
            binding.unbind();
        }
    }

    pub fn set_active(&self, active: bool) {
        self.imp().set_active(active);
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

    /// Recompute the collapsed subtitle (Pango markup) based on the
    /// current `peer-commit` property and the local build's commit.
    /// Soft-warn semantics: a missing or mismatched peer commit
    /// surfaces as orange text but never blocks traffic. Called by
    /// the window after `update_client_state` writes the new
    /// `peer-commit`. The dns-status icon is left to its existing
    /// `set_dns_state` handler so the two indicators don't fight
    /// for the same CSS class.
    /// Recompute the collapsed title, status badge, connect/disconnect button,
    /// and subtitle: connection state first, build mismatch only as a warning suffix.
    pub fn refresh_status(&self) {
        let data = self.imp().client_object.borrow().as_ref().map(|co| {
            (
                co.property::<bool>("active"),
                co.property::<bool>("alive"),
                co.property::<Option<String>>("active-addr"),
                co.property::<Option<String>>("incoming-addr"),
                co.property::<u32>("rtt-ms"),
                co.property::<Option<String>>("peer-commit"),
                co.property::<Option<String>>("hostname"),
                co.property::<Vec<String>>("ips"),
                co.property::<String>("position"),
            )
        });
        let Some((active, alive, addr, incoming_addr, rtt, peer, hostname, ips, position)) = data
        else {
            return;
        };

        // Compute row title: prefer hostname, fall back to first IP, then placeholder.
        let primary_ip = ips.first().cloned();
        let title_markup = match (
            hostname.as_deref().map(str::trim).filter(|s| !s.is_empty()),
            primary_ip.as_deref(),
        ) {
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

        self.remove_css_class("peer-mismatch");
        self.remove_css_class("peer-match");
        self.remove_css_class("peer-unknown");
        self.remove_css_class("client-offline");
        self.remove_css_class("client-waiting");

        let badge = &self.imp().status_badge;
        badge.remove_css_class("status-connected");
        badge.remove_css_class("status-waiting");
        badge.remove_css_class("status-offline");

        let connect_btn = &self.imp().connect_button;
        if active {
            connect_btn.set_label("Disconnect");
            connect_btn.remove_css_class("suggested-action");
        } else {
            connect_btn.set_label("Connect");
            connect_btn.add_css_class("suggested-action");
        }

        let addr = addr.map(|a| glib::markup_escape_text(&a).to_string());
        let incoming = incoming_addr.map(|a| glib::markup_escape_text(&a).to_string());

        let markup = match (active, addr, alive, incoming) {
            (true, Some(addr), true, Some(_)) => {
                self.add_css_class("peer-match");
                badge.set_label("Connected");
                badge.add_css_class("status-connected");
                format!("Connected (bidirectional) · {addr} · Screen: {pos_label}{rtt_suffix}")
            }
            (true, Some(addr), true, None) => {
                self.add_css_class("peer-match");
                badge.set_label("Connected");
                badge.add_css_class("status-connected");
                format!("Connected to {addr} · Screen: {pos_label}{rtt_suffix}")
            }
            (true, _, _, Some(in_addr)) => {
                self.add_css_class("peer-match");
                badge.set_label("Connected");
                badge.add_css_class("status-connected");
                format!("Connected from {in_addr} · Screen: {pos_label}{rtt_suffix}")
            }
            // Active but no usable connection yet: either the peer has not
            // authorized this device or it is still booting.
            (true, None, _, None) => {
                self.add_css_class("client-waiting");
                badge.set_label("Waiting approval");
                badge.add_css_class("status-waiting");
                format!(
                    "Waiting for the other device — approve the request there · Screen: {pos_label}"
                )
            }
            (true, Some(_), false, None) => {
                self.add_css_class("client-waiting");
                badge.set_label("Reconnecting");
                badge.add_css_class("status-waiting");
                format!("Connection lost — reconnecting · Screen: {pos_label}{rtt_suffix}")
            }
            (false, _, _, Some(in_addr)) => {
                self.add_css_class("peer-match");
                badge.set_label("Receiving");
                badge.add_css_class("status-connected");
                format!("Receiving input from {in_addr} (outgoing disabled) · Screen: {pos_label}")
            }
            (false, _, _, None) => {
                self.add_css_class("client-offline");
                badge.set_label("Disconnected");
                badge.add_css_class("status-offline");
                format!("Disconnected · Screen: {pos_label} — click Connect to pair")
            }
        };

        // Only mention the build when it actually differs: matching builds
        // are the normal case and the information is noise.
        let markup = match peer.as_deref() {
            Some(p) if p != local.as_str() => {
                self.add_css_class("peer-mismatch");
                let p = glib::markup_escape_text(p);
                format!("{markup} · peer build {p} (ours {local})")
            }
            _ => markup,
        };

        self.set_subtitle(&markup);
    }
}
