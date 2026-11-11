# Lan Mouse cursor hiding extension (GNOME Shell, Wayland)

GNOME Wayland does not expose a normal-client API to hide the system cursor.
This tiny extension does it from inside the Shell while Lan Mouse controls a
peer.

The service writes/touches `$XDG_RUNTIME_DIR/lan-mouse/cursor-hide` while an
outgoing capture is active. The extension hides the cursor while that file is
fresh (3 s heartbeat) and restores it when the file disappears or goes stale.

The app can install it into
`~/.local/share/gnome-shell/extensions/lan-mouse-cursor@lan-mouse-custom/`
and add the UUID to `org.gnome.shell enabled-extensions`. GNOME Wayland loads
newly installed extensions only after a **logout/login**.
