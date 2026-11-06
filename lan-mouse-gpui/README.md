# lan-mouse-gpui (prototype)

Prototype of the GPUI replacement for the GTK frontend. It is deliberately a
standalone package (its own `[workspace]`) while the GTK frontend is still the
shipping UI; it builds through `.github/workflows/gpui-frontend.yml` on Linux
and Windows MSVC.

Current slice:
- GPUI shell with sidebar (Devices/Network/Settings) and a real IPC bridge to
  the running Lan Mouse service;
- `Enumerate`/`Created`/`State`/`Deleted` events populate device cards;
- Connect/Disconnect sends `Activate`; Add device sends `Create`;
- service/unknown-event errors are surfaced in the sidebar status.

Not implemented yet: dialogs, permissions, network scan, clipboard/pairing
settings, tray integration, theme/responsive polish, tests and eventual
replacement of `lan-mouse-gtk` in the main binary.
