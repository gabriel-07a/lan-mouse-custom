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
frontend="$target/release/lan-mouse-gpui"
deb_arch="$(dpkg --print-architecture 2>/dev/null || echo amd64)"
out="lan-mouse-custom_${version}_${deb_arch}.deb"

[ -x "$binary" ] || {
    echo "missing $binary: run 'cargo build --release' first" >&2
    exit 1
}
[ -x "$frontend" ] || {
    echo "missing $frontend: build lan-mouse-gpui in release mode first" >&2
    exit 1
}

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

install -Dm755 "$binary" "$stage/usr/bin/lan-mouse"
install -Dm755 "$frontend" "$stage/usr/bin/lan-mouse-gpui"
install -Dm644 de.feschber.LanMouse.desktop "$stage/usr/share/applications/de.feschber.LanMouse.desktop"
install -Dm644 lan-mouse-gtk/resources/de.feschber.LanMouse.svg \
    "$stage/usr/share/icons/hicolor/scalable/apps/de.feschber.LanMouse.svg"

# Raster sizes give the taskbar/tray crisp icons at every scale.
for size in 32 48 128 256 512; do
    install -Dm644 "assets/icon-${size}.png" \
        "$stage/usr/share/icons/hicolor/${size}x${size}/apps/de.feschber.LanMouse.png"
done

# systemd user unit: enable with `systemctl --user enable --now lan-mouse.service`.
install -Dm644 service/lan-mouse.service \
    "$stage/usr/lib/systemd/user/lan-mouse.service"
# License text is required by Debian policy; upstream ships GPL-3.0-or-later.
install -Dm644 LICENSE "$stage/usr/share/doc/lan-mouse/copyright"
install -Dm644 NOTICE "$stage/usr/share/doc/lan-mouse/NOTICE"

installed_size="$(du -sk "$stage" | cut -f1)"
mkdir -p "$stage/DEBIAN"
cat > "$stage/DEBIAN/control" <<EOF
Package: lan-mouse
Version: $version
Section: utils
Priority: optional
Architecture: ${deb_arch}
Depends: libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libx11-6, libxcb1, libfontconfig1, libfreetype6, libvulkan1
Recommends: gnome-shell-extension-appindicator, mesa-vulkan-drivers
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
    # The shortcut was created as <Desktop>/de.feschber.LanMouse.desktop,
    # i.e. depth 3 below /home; the previous -maxdepth 2 never matched it.
    find /home -maxdepth 4 -type f -name de.feschber.LanMouse.desktop -delete >/dev/null 2>&1 || true
    rm -f /root/Desktop/de.feschber.LanMouse.desktop >/dev/null 2>&1 || true
fi
exit 0
PRERM
chmod 755 "$stage/DEBIAN/prerm"

# Per-user data (devices, permissions, certificate, audit log, portal consent
# tokens) is NEVER touched by a maintainer script: dpkg runs as root, cannot
# know which users ran Lan Mouse and must not modify home directories behind
# the user's back. Upgrades therefore keep the configuration on purpose, even
# on `apt purge`. The postrm only explains how to reset it explicitly:
#   lan-mouse --reset-config          (moves it to a reversible .bak backup;
#                                      add --purge to delete permanently)
#   scripts/install-clean.sh --yes    (from the source tree)
cat > "$stage/DEBIAN/postrm" <<'POSTRM'
#!/bin/sh
set -e
if [ "$1" = "purge" ]; then
    printf '%s\n' \
        "lan-mouse: per-user configuration was kept (this is intentional):" \
        "  ~/.config/lan-mouse        devices, permissions, certificate" \
        "  ~/.local/state/lan-mouse   audit log and portal consent tokens" \
        "  ~/.cache/lan-mouse         portal tokens from older builds" \
        "Run 'lan-mouse --reset-config' before uninstalling to move that data" \
        "to a reversible 'lan-mouse.bak-<timestamp>' backup ('--purge' deletes" \
        "it permanently), or delete the directories manually for a clean state." >&2
fi
# Refresh the desktop/icon caches after the package files are gone.
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t /usr/share/icons/hicolor >/dev/null 2>&1 || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database /usr/share/applications >/dev/null 2>&1 || true
fi
exit 0
POSTRM
chmod 755 "$stage/DEBIAN/postrm"

dpkg-deb --root-owner-group --build "$stage" "$out"
echo "built $out"
