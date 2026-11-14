# lan-mouse-gpui

The graphical frontend of Lan Mouse (the legacy GTK frontend has been removed).
It is a standalone package (its own `[workspace]`) launched by the main binary
as a separate process; it builds through `.github/workflows/gpui-frontend.yml`
on Linux and Windows MSVC.

Current slice:
- GPUI shell with sidebar (Devices/Network/Settings) and a real IPC bridge to
  the running Lan Mouse service;
- `Enumerate`/`Created`/`State`/`Deleted` events populate device cards;
- Connect/Disconnect sends `Activate`; Add device sends `Create`;
- service/unknown-event errors are surfaced in the sidebar status.

Not implemented yet: dialogs, permissions, network scan, clipboard/pairing
settings, tray integration, theme/responsive polish and tests.
