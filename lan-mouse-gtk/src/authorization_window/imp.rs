use std::sync::OnceLock;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::subclass::InitializingObject;
use gtk::{
    Button, CheckButton, CompositeTemplate, Label, Switch,
    glib::{self, clone, subclass::Signal},
    template_callbacks,
};

use lan_mouse_ipc::Capabilities;

#[derive(CompositeTemplate, Default)]
#[template(resource = "/de/feschber/LanMouse/authorization_window.ui")]
pub struct AuthorizationWindow {
    #[template_child]
    pub fingerprint: TemplateChild<Label>,
    #[template_child]
    pub position: TemplateChild<adw::ComboRow>,
    #[template_child]
    pub peer: TemplateChild<Label>,
    #[template_child]
    pub cancel_button: TemplateChild<Button>,
    #[template_child]
    pub confirm_button: TemplateChild<Button>,
    #[template_child]
    pub incoming_pointer: TemplateChild<CheckButton>,
    #[template_child]
    pub incoming_keyboard: TemplateChild<CheckButton>,
    #[template_child]
    pub incoming_clipboard: TemplateChild<CheckButton>,
    #[template_child]
    pub outgoing_switch: TemplateChild<Switch>,
    #[template_child]
    pub outgoing_pointer: TemplateChild<CheckButton>,
    #[template_child]
    pub outgoing_keyboard: TemplateChild<CheckButton>,
    #[template_child]
    pub outgoing_clipboard: TemplateChild<CheckButton>,
}

/// Pack the state of three capability checkboxes into a bitmask understood by
/// the window (see `policy_from_mask` there). `0` means "nothing granted".
fn capability_mask(pointer: &CheckButton, keyboard: &CheckButton, clipboard: &CheckButton) -> u32 {
    let mut bits = 0u8;
    if pointer.is_active() {
        bits |= Capabilities::POINTER.0;
    }
    if keyboard.is_active() {
        bits |= Capabilities::KEYBOARD.0;
    }
    if clipboard.is_active() {
        bits |= Capabilities::CLIPBOARD.0;
    }
    u32::from(bits)
}

#[glib::object_subclass]
impl ObjectSubclass for AuthorizationWindow {
    const NAME: &'static str = "AuthorizationWindow";
    const ABSTRACT: bool = false;

    type Type = super::AuthorizationWindow;
    type ParentType = adw::Window;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
        klass.bind_template_callbacks();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

#[template_callbacks]
impl AuthorizationWindow {
    #[template_callback]
    fn handle_confirm(&self, _button: Button) {
        let fp = self.fingerprint.text().as_str().trim().to_owned();
        let position = self.position.selected();
        let incoming = capability_mask(
            &self.incoming_pointer,
            &self.incoming_keyboard,
            &self.incoming_clipboard,
        );
        // A disabled outgoing direction means "this PC must not control the
        // other one", regardless of the individual boxes.
        let outgoing = if self.outgoing_switch.is_active() {
            capability_mask(
                &self.outgoing_pointer,
                &self.outgoing_keyboard,
                &self.outgoing_clipboard,
            )
        } else {
            0
        };
        self.obj()
            .emit_by_name::<()>("confirm-clicked", &[&fp, &position, &incoming, &outgoing])
    }

    #[template_callback]
    fn handle_cancel(&self, _: Button) {
        let fp = self.fingerprint.text().as_str().trim().to_owned();
        self.obj().emit_by_name::<()>("cancel-clicked", &[&fp])
    }

    pub(super) fn set_fingerprint(&self, fingerprint: &str) {
        self.fingerprint.set_text(fingerprint);
    }

    pub(super) fn set_peer(&self, addr: std::net::SocketAddr, hostname: Option<&str>) {
        match hostname.map(str::trim).filter(|h| !h.is_empty()) {
            Some(host) => self.peer.set_text(&format!(
                "Device \"{host}\" ({}) wants to connect.",
                addr.ip()
            )),
            None => self
                .peer
                .set_text(&format!("Device at {} wants to connect.", addr.ip())),
        }
    }
}

impl ObjectImpl for AuthorizationWindow {
    fn constructed(&self) {
        self.parent_constructed();
        // The individual outgoing capability checkboxes are meaningless while
        // the whole direction is switched off, so grey them out instead of
        // silently ignoring their state.
        self.outgoing_switch.connect_notify_local(
            Some("active"),
            clone!(
                #[weak(rename_to = imp)]
                self,
                move |switch, _| {
                    let sensitive = switch.is_active();
                    imp.outgoing_pointer.set_sensitive(sensitive);
                    imp.outgoing_keyboard.set_sensitive(sensitive);
                    imp.outgoing_clipboard.set_sensitive(sensitive);
                }
            ),
        );
    }

    fn signals() -> &'static [Signal] {
        static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
        SIGNALS.get_or_init(|| {
            vec![
                Signal::builder("confirm-clicked")
                    .param_types([
                        String::static_type(),
                        u32::static_type(),
                        u32::static_type(),
                        u32::static_type(),
                    ])
                    .build(),
                Signal::builder("cancel-clicked")
                    .param_types([String::static_type()])
                    .build(),
            ]
        })
    }
}

impl WidgetImpl for AuthorizationWindow {}
impl WindowImpl for AuthorizationWindow {}
impl ApplicationWindowImpl for AuthorizationWindow {}
impl AdwWindowImpl for AuthorizationWindow {}
