//! System tray for Linux desktops.
//!
//! GNOME does not implement `org.kde.StatusNotifierItem` itself, but Ubuntu
//! ships the `ubuntu-appindicators` extension (enabled by default), and KDE,
//! XFCE, Cinnamon and Budgie implement it natively. Registering an SNI item
//! over D-Bus therefore works without pulling in a C appindicator library that
//! would clash with GTK4.
//!
//! The item exposes a small `com.canonical.dbusmenu` with "Open" and "Quit".

use std::{cell::RefCell, collections::HashMap};

use gtk::{
    gio,
    glib::{self, clone},
    prelude::*,
};

use crate::{tray_menu, window::Window};

const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_IFACE: &str = "org.kde.StatusNotifierWatcher";

const ITEM_XML: &str = r#"<node>
  <interface name="org.kde.StatusNotifierItem">
    <method name="Activate">
      <arg type="i" direction="in"/>
      <arg type="i" direction="in"/>
    </method>
    <method name="SecondaryActivate">
      <arg type="i" direction="in"/>
      <arg type="i" direction="in"/>
    </method>
    <method name="ContextMenu">
      <arg type="i" direction="in"/>
      <arg type="i" direction="in"/>
    </method>
    <property name="Category" type="s" access="read"/>
    <property name="Id" type="s" access="read"/>
    <property name="Title" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="IconName" type="s" access="read"/>
    <property name="Menu" type="o" access="read"/>
    <property name="ItemIsMenu" type="b" access="read"/>
  </interface>
</node>"#;

const MENU_XML: &str = r#"<node>
  <interface name="com.canonical.dbusmenu">
    <method name="GetLayout">
      <arg type="i" direction="in"/>
      <arg type="i" direction="in"/>
      <arg type="as" direction="in"/>
      <arg type="u" direction="out"/>
      <arg type="(ia{sv}av)" direction="out"/>
    </method>
    <method name="GetGroupProperties">
      <arg type="ai" direction="in"/>
      <arg type="as" direction="in"/>
      <arg type="a(ia{sv})" direction="out"/>
    </method>
    <method name="Event">
      <arg type="i" direction="in"/>
      <arg type="s" direction="in"/>
      <arg type="v" direction="in"/>
    </method>
    <method name="AboutToShow">
      <arg type="i" direction="in"/>
      <arg type="b" direction="out"/>
    </method>
    <property name="Version" type="u" access="read"/>
    <property name="TextDirection" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
  </interface>
</node>"#;

use tray_menu::{OPEN_ID as MENU_OPEN, QUIT_ID as MENU_QUIT};

thread_local! {
    /// Keeps the D-Bus connection (and therefore the registration) alive.
    static CONNECTION: RefCell<Option<gio::DBusConnection>> = const { RefCell::new(None) };
}

/// Register the tray icon. Fails softly: a desktop without the extension
/// simply has no tray icon.
pub(crate) fn setup(window: &Window) {
    if let Err(e) = try_setup(window) {
        log::warn!("system tray unavailable: {e}");
    }
}

fn try_setup(window: &Window) -> Result<(), glib::Error> {
    let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;
    let bus_name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());

    let item_info = gio::DBusNodeInfo::for_xml(ITEM_XML)?;
    let menu_info = gio::DBusNodeInfo::for_xml(MENU_XML)?;

    // --- org.kde.StatusNotifierItem -------------------------------------
    {
        let window = window.clone();
        connection
            .register_object(ITEM_PATH, &item_info.interfaces()[0])
            .method_call(
                move |_conn, _sender, _path, _iface, method, _params, invocation| {
                    if method == "Activate" || method == "SecondaryActivate" {
                        window.present();
                    }
                    invocation.return_value(None);
                },
            )
            .property(|_conn, _sender, _path, _iface, property| match property {
                "Category" => "ApplicationStatus".to_variant(),
                "Id" => "lan-mouse".to_variant(),
                "Title" => "Lan Mouse".to_variant(),
                "Status" => "Active".to_variant(),
                "IconName" => tray_menu::APP_ICON.to_variant(),
                "Menu" => glib::variant::ObjectPath::try_from(MENU_PATH.to_string())
                    .expect("valid object path")
                    .to_variant(),
                "ItemIsMenu" => true.to_variant(),
                _ => false.to_variant(),
            })
            .build()?;
    }

    // --- com.canonical.dbusmenu -----------------------------------------
    {
        let window = window.clone();
        connection
            .register_object(MENU_PATH, &menu_info.interfaces()[0])
            .method_call(
                move |_conn, _sender, _path, _iface, method, params, invocation| {
                    match method {
                        "GetLayout" => invocation.return_value(Some(&layout().to_variant())),
                        // Out arguments are always a tuple, even for one value.
                        "GetGroupProperties" => invocation.return_value(Some(
                            &(tray_menu::ITEMS
                                .iter()
                                .map(|(id, label)| (*id, item_properties(label)))
                                .collect::<Vec<_>>(),)
                                .to_variant(),
                        )),
                        "Event" => {
                            if let Some((id, event)) = params.get::<(i32, String)>() {
                                if event == "clicked" {
                                    match id {
                                        MENU_OPEN => window.present(),
                                        MENU_QUIT => {
                                            if let Some(app) = window.application() {
                                                app.activate_action(tray_menu::QUIT_ACTION, None);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            invocation.return_value(None);
                        }
                        "AboutToShow" => invocation.return_value(Some(&(false,).to_variant())),
                        _ => invocation.return_value(None),
                    }
                },
            )
            .property(|_conn, _sender, _path, _iface, property| match property {
                "Version" => 3u32.to_variant(),
                "TextDirection" => "ltr".to_variant(),
                "Status" => "normal".to_variant(),
                _ => 0u32.to_variant(),
            })
            .build()?;
    }

    // Own the name and announce the item to the watcher.
    let registered_name = bus_name.clone();
    gio::bus_own_name_on_connection(
        &connection,
        &bus_name,
        gio::BusNameOwnerFlags::NONE,
        clone!(
            #[weak]
            connection,
            move |_conn, _name| {
                // Parameters must be a tuple, not a bare string.
                let parameters = (registered_name.as_str(),).to_variant();
                let result = connection.call_sync(
                    Some(WATCHER),
                    WATCHER_PATH,
                    WATCHER_IFACE,
                    "RegisterStatusNotifierItem",
                    Some(&parameters),
                    None,
                    gio::DBusCallFlags::NONE,
                    -1,
                    gio::Cancellable::NONE,
                );
                match result {
                    Ok(_) => log::info!("system tray registered"),
                    Err(e) => log::warn!("system tray watcher refused the item: {e}"),
                }
            }
        ),
        |_, _| log::warn!("system tray bus name lost"),
    );

    CONNECTION.with(|slot| slot.replace(Some(connection)));
    Ok(())
}

type MenuItem = (i32, HashMap<String, glib::Variant>);

fn item_properties(label: &str) -> HashMap<String, glib::Variant> {
    let mut props = HashMap::new();
    props.insert("label".to_string(), label.to_variant());
    props.insert("enabled".to_string(), true.to_variant());
    props.insert("visible".to_string(), true.to_variant());
    props
}

/// `(revision, (id, properties, children))` for the root menu.
type Layout = (
    u32,
    (i32, HashMap<String, glib::Variant>, Vec<glib::Variant>),
);

fn layout() -> Layout {
    let mut root = HashMap::new();
    root.insert("children-display".to_string(), "submenu".to_variant());

    let children = tray_menu::ITEMS
        .iter()
        .map(|(id, label)| (*id, item_properties(label), Vec::<glib::Variant>::new()).to_variant())
        .collect();

    (1, (0, root, children))
}

#[allow(dead_code)]
fn _assert_menu_item_type(_: MenuItem) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_exposes_the_shared_menu_items() {
        let (revision, (root_id, root_props, children)) = layout();
        assert_eq!(revision, 1);
        assert_eq!(root_id, 0);
        assert_eq!(
            root_props
                .get("children-display")
                .and_then(|v| v.get::<String>()),
            Some("submenu".to_string())
        );
        assert_eq!(children.len(), tray_menu::ITEMS.len());

        for (child, (id, label)) in children.iter().zip(tray_menu::ITEMS) {
            let (child_id, props, _) = child
                .get::<(i32, HashMap<String, glib::Variant>, Vec<glib::Variant>)>()
                .expect("menu item");
            assert_eq!(child_id, id);
            assert_eq!(
                props.get("label").and_then(|v| v.get::<String>()),
                Some(label.to_string())
            );
            assert_eq!(
                props.get("visible").and_then(|v| v.get::<bool>()),
                Some(true)
            );
            assert_eq!(
                props.get("enabled").and_then(|v| v.get::<bool>()),
                Some(true)
            );
        }
    }

    #[test]
    fn the_quit_id_maps_to_the_quit_action() {
        // Guards against swapping the ids in the shared table.
        assert_eq!(tray_menu::QUIT_ID, 2);
        assert_eq!(tray_menu::OPEN_ID, 1);
    }
}
