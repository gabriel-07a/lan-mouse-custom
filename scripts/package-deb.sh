#!/usr/bin/env bash
# Build a Debian package without cargo-bundle.
#
# cargo-bundle 0.12 pulls in sigstore, which no longer compiles with current
# rustc, so the release pipeline packages the already built binary directly.
#
# Usage: scripts/package-deb.sh <version> [target-dir]
set -euo pipefail

version="${1:?usage: package-deb.sh <version> [target-dir]}"
target="${2:-target}"
binary="$target/release/lan-mouse"
out="lan-mouse-custom_${version}_amd64.deb"

[ -x "$binary" ] || {
    echo "missing $binary: run 'cargo build --release' first" >&2
    exit 1
}

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

install -Dm755 "$binary" "$stage/usr/bin/lan-mouse"
install -Dm644 de.feschber.LanMouse.desktop "$stage/usr/share/applications/de.feschber.LanMouse.desktop"
install -Dm644 lan-mouse-gtk/resources/de.feschber.LanMouse.svg \
    "$stage/usr/share/icons/hicolor/scalable/apps/de.feschber.LanMouse.svg"

# Raster sizes give the taskbar/tray crisp icons at every scale.
for size in 32 48 128 256 512; do
    install -Dm644 "assets/icon-${size}.png" \
        "$stage/usr/share/icons/hicolor/${size}x${size}/apps/de.feschber.LanMouse.png"
done

installed_size="$(du -sk "$stage" | cut -f1)"
mkdir -p "$stage/DEBIAN"
cat > "$stage/DEBIAN/control" <<EOF
Package: lan-mouse
Version: $version
Section: utils
Priority: optional
Architecture: amd64
Depends: libgtk-4-1, libadwaita-1-0
Recommends: gnome-shell-extension-appindicator
Installed-Size: $installed_size
Maintainer: Gabriel Arcanjo <gabrielarcanjo1907@gmail.com>
Description: Software KVM switch (unofficial Lan Mouse fork)
 Share one mouse and keyboard between computers on the local network.
 This is a modified, unofficial fork of Lan Mouse with clipboard sync
 and network discovery.
Homepage: https://github.com/gabriel-07a/lan-mouse-custom
License: GPL-3.0-or-later
EOF

# postinst/prerm: refresh caches and keep a desktop shortcut in sync.
cat > "$stage/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
set -e
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t /usr/share/icons/hicolor >/dev/null 2>&1 || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database /usr/share/applications >/dev/null 2>&1 || true
fi

# Drop a shortcut on the desktop of the user that installed the package.
user="${SUDO_USER:-}"
if [ -z "$user" ] && [ -n "${PKEXEC_UID:-}" ]; then
    user="$(getent passwd "$PKEXEC_UID" | cut -d: -f1)"
fi
if [ -n "$user" ]; then
    home="$(getent passwd "$user" | cut -d: -f6)"
    if [ -n "$home" ]; then
        desktop_dir="$(sudo -u "$user" xdg-user-dir DESKTOP 2>/dev/null || true)"
        [ -n "$desktop_dir" ] || desktop_dir="$home/Desktop"
        if [ -d "$desktop_dir" ]; then
            cp /usr/share/applications/de.feschber.LanMouse.desktop "$desktop_dir/" 2>/dev/null || true
            chown "$user": "$desktop_dir/de.feschber.LanMouse.desktop" 2>/dev/null || true
            chmod +x "$desktop_dir/de.feschber.LanMouse.desktop" 2>/dev/null || true
            sudo -u "$user" gio set "$desktop_dir/de.feschber.LanMouse.desktop" \
                metadata::trusted true >/dev/null 2>&1 || true
        fi
    fi
fi
exit 0
POSTINST
chmod 755 "$stage/DEBIAN/postinst"

cat > "$stage/DEBIAN/prerm" <<'PRERM'
#!/bin/sh
set -e
if [ "$1" = "remove" ] || [ "$1" = "purge" ]; then
    pkill -f '/usr/bin/lan-mouse' >/dev/null 2>&1 || true
    find /home -maxdepth 2 -name de.feschber.LanMouse.desktop -delete >/dev/null 2>&1 || true
fi
exit 0
PRERM
chmod 755 "$stage/DEBIAN/prerm"

dpkg-deb --root-owner-group --build "$stage" "$out"
echo "built $out"
