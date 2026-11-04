#!/bin/sh
# Generate the application icons used by the Windows executable, the NSIS
# installer, the Windows tray, the .deb and the Linux tray.
#
# Same geometry as scripts/makeicns.sh (the Big Sur+ template): a 1024 canvas
# with an 824 white squircle (radius 185) and the artwork centred at 560.
#
# Requires rsvg-convert and ImageMagick (`magick`/`convert`).
#
# Usage: scripts/makeicons.sh [SVG]
set -e

svg="${1:-./lan-mouse-gtk/resources/de.feschber.LanMouse.svg}"
outdir="./assets"

CANVAS=1024
SQUIRCLE=824
CONTENT=560
RADIUS=185
BG_COLOR="#FFFFFF"
SQUIRCLE_OFFSET=$(( (CANVAS - SQUIRCLE) / 2 ))
CONTENT_OFFSET=$(( (CANVAS - CONTENT) / 2 ))

if ! command -v rsvg-convert >/dev/null 2>&1; then
    echo "makeicons: rsvg-convert is required (librsvg2-bin)" >&2
    exit 1
fi

magick=""
for candidate in magick convert; do
    if command -v "$candidate" >/dev/null 2>&1; then
        magick="$candidate"
        break
    fi
done
if [ -z "$magick" ]; then
    echo "makeicons: ImageMagick (magick or convert) is required" >&2
    exit 1
fi

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

rsvg-convert -w "$CONTENT" -h "$CONTENT" "$svg" -o "$workdir/content.png"

"$magick" -size ${CANVAS}x${CANVAS} xc:none \
    -fill "$BG_COLOR" \
    -draw "roundrectangle ${SQUIRCLE_OFFSET},${SQUIRCLE_OFFSET} $((CANVAS-SQUIRCLE_OFFSET-1)),$((CANVAS-SQUIRCLE_OFFSET-1)) $RADIUS,$RADIUS" \
    "$workdir/background.png"

"$magick" "$workdir/background.png" \
    "$workdir/content.png" -geometry +${CONTENT_OFFSET}+${CONTENT_OFFSET} -composite \
    -colorspace sRGB -type TrueColorAlpha PNG32:"$workdir/icon-1024.png"

mkdir -p "$outdir"
for size in 512 256 128 64 48 32 16; do
    "$magick" "$workdir/icon-1024.png" -resize ${size}x${size} \
        -colorspace sRGB -type TrueColorAlpha PNG32:"$workdir/${size}.png"
done

# PNGs for the .deb (hicolor) and the Linux tray.
for size in 512 256 128 48 32; do
    cp "$workdir/${size}.png" "$outdir/icon-${size}.png"
done

# The tray loads the icon from the GTK resource bundle, so keep a copy there.
mkdir -p lan-mouse-gtk/resources/icons
cp "$outdir/icon-256.png" lan-mouse-gtk/resources/icons/icon-256.png

# Multi-resolution .ico for the executable, the installer and the tray.
"$magick" "$workdir/16.png" "$workdir/32.png" "$workdir/48.png" \
    "$workdir/64.png" "$workdir/128.png" "$workdir/256.png" \
    "$outdir/icon.ico"

echo "wrote $outdir/icon.ico and $outdir/icon-{32,48,128,256,512}.png"
