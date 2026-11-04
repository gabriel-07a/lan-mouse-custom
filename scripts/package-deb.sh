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

installed_size="$(du -sk "$stage" | cut -f1)"
mkdir -p "$stage/DEBIAN"
cat > "$stage/DEBIAN/control" <<EOF
Package: lan-mouse
Version: $version
Section: utils
Priority: optional
Architecture: amd64
Depends: libgtk-4-1, libadwaita-1-0
Installed-Size: $installed_size
Maintainer: Gabriel Arcanjo <gabrielarcanjo1907@gmail.com>
Description: Software KVM switch (unofficial Lan Mouse fork)
 Share one mouse and keyboard between computers on the local network.
 This is a modified, unofficial fork of Lan Mouse with clipboard sync
 and network discovery.
Homepage: https://github.com/gabriel-07a/lan-mouse-custom
License: GPL-3.0-or-later
EOF

dpkg-deb --root-owner-group --build "$stage" "$out"
echo "built $out"
