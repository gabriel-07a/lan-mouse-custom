#!/usr/bin/env bash
# Screenshots of the GPUI frontend in demo mode (LAN_MOUSE_GPUI_DEMO=1), taken
# in a throwaway headless GNOME Shell (scripts/gnome-headless.sh) at several
# window sizes, so layout problems (clipped or overlapping elements) can be
# reviewed without touching the desktop.
#
# Usage: scripts/gui-screenshots.sh <out-dir> [gui-binary]
#   sizes: GUI_SIZES (default "1920x1080 1366x768 1024x700")
#   pages: GUI_PAGES (default "devices network settings add"; add = the Add
#          device dialog)
#   language: GUI_LANGUAGE (en | pt-br, default en)
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$(realpath -m "${1:?usage: $0 <out-dir> [gui-binary]}")"
gui="$(realpath "${2:-$repo/lan-mouse-gpui/target/debug/lan-mouse-gpui}")"
mkdir -p "$out"

for size in ${GUI_SIZES:-1920x1080 1366x768 1024x700}; do
    inner="$(mktemp)"
    cat >"$inner" <<INNER
#!/usr/bin/env bash
set -u
for page in ${GUI_PAGES:-devices network settings add}; do
    LAN_MOUSE_GPUI_DEMO=1 LAN_MOUSE_GPUI_PAGE=\$page LAN_MOUSE_GPUI_LANGUAGE=${GUI_LANGUAGE:-en} "$gui" >"$out/gui-\$page-$size.log" 2>&1 &
    pid=\$!
    sleep 4
    shot="$out/\$page-$size.png"
    rm -f "\$shot" "\$shot.done"
    printf '%s' "\$shot" >"\$LAN_MOUSE_TEST_PROBE.shot"
    for _ in \$(seq 1 100); do [ -f "\$shot.done" ] && break; sleep 0.1; done
    echo "\$page $size: \$(cat "\$shot.done" 2>/dev/null || echo 'no screenshot')"
    rm -f "\$shot.done"
    kill \$pid 2>/dev/null; wait \$pid 2>/dev/null
done
INNER
    chmod +x "$inner"
    GNOME_HEADLESS_SIZE="$size" "$repo/scripts/gnome-headless.sh" "$inner" 2>/dev/null | grep -E '^(devices|network|settings|add) ' || true
    rm -f "$inner"
done
