# Clipboard synchronization QA evidence

## R-15: clipboard unavailable with "Invalid state" on GNOME

Reported: Ctrl+C/Ctrl+V (text and images) did nothing. The journal showed every clipboard backend
unavailable, with the portal backend failing at RequestClipboard with
org.freedesktop.DBus.Error.Failed: Invalid state.

Root cause is a stale xdg-desktop-portal frontend: the frontend caches the backend RemoteDesktop version when
it starts, and the GNOME backend's version check (>= 2) is what allows clipboard requests. After a
portal-gnome update/restart the running frontend can keep the cached old version and rejects every
RequestClipboard with "Invalid state"; restarting xdg-desktop-portal itself repairs it (verified live:
RequestClipboard returned Ok after restarting xdg-desktop-portal and xdg-desktop-portal-gnome).

The clipboard backend now recognises this error and logs an actionable message (restart the portal or log
out/in) instead of only "request clipboard: ... Invalid state". lan-mouse doctor reports the Clipboard portal
version and whether consent token is present ("clipboard portal: v1 (consent saved)").
