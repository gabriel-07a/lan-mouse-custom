#![cfg_attr(windows, allow(dead_code))]

//! Shared definition of the system tray menu.
//!
//! Both the Windows (`Shell_NotifyIconW`) and the Linux
//! (`StatusNotifierItem`/`dbusmenu`) implementations expose the same two
//! entries. Keeping the ids, labels and the action name here means a rename
//! cannot silently break one platform, and it makes the menu testable without
//! a display.

/// Icon theme name installed by the `.deb` and referenced by the `.desktop`.
pub(crate) const APP_ICON: &str = "de.feschber.LanMouse";

/// Action registered on the `GApplication` by the frontend.
pub(crate) const QUIT_ACTION: &str = "quit-app";

pub(crate) const OPEN_ID: i32 = 1;
pub(crate) const QUIT_ID: i32 = 2;

pub(crate) const OPEN_LABEL: &str = "Open Lan Mouse";
pub(crate) const QUIT_LABEL: &str = "Quit Lan Mouse";

/// `(id, label)` of every entry, in display order.
pub(crate) const ITEMS: [(i32, &str); 2] = [(OPEN_ID, OPEN_LABEL), (QUIT_ID, QUIT_LABEL)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_has_open_and_quit_with_distinct_ids() {
        assert_eq!(ITEMS.len(), 2);
        assert_eq!(ITEMS[0], (OPEN_ID, OPEN_LABEL));
        assert_eq!(ITEMS[1], (QUIT_ID, QUIT_LABEL));
        assert_ne!(OPEN_ID, QUIT_ID);
        assert!(ITEMS.iter().all(|(_, label)| !label.trim().is_empty()));
    }

    #[test]
    fn quit_action_name_is_the_one_the_menu_uses() {
        assert_eq!(QUIT_ACTION, "quit-app");
        // The identifier must be a valid GAction name.
        assert!(
            QUIT_ACTION
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
    }

    #[test]
    fn app_icon_matches_the_packaged_icon_name() {
        assert_eq!(APP_ICON, "de.feschber.LanMouse");
    }
}
