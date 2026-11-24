#!/usr/bin/env bash
# Run a command inside a throwaway headless GNOME Shell (Wayland, one virtual
# monitor, Ubuntu session mode so the Ubuntu Dock is loaded).
#
# Nothing touches the user's desktop: the shell gets its own D-Bus session,
# runtime dir, dconf database, config/data/cache dirs and no input devices
# (headless). The test probe extension (tests/gnome-headless/) reports the
# pointer and the dock state to $LAN_MOUSE_TEST_PROBE.
#
# Usage:
#   scripts/gnome-headless.sh <command> [args...]
#   scripts/gnome-headless.sh --test     # build and run the GNOME e2e tests
#
# GNOME_HEADLESS_SIZE: space-separated virtual monitors (default 1280x800),
#   e.g. "2560x1600 2560x1440"; they are Meta-0, Meta-1, ...
# GNOME_HEADLESS_MONITORS: a monitors.xml giving their scale and layout
#   (tests/gnome-headless/*.xml); fractional scales are enabled like on
#   Ubuntu (scale-monitor-framebuffer).
#
# Environment inside the command: WAYLAND_DISPLAY, XDG_RUNTIME_DIR,
# DBUS_SESSION_BUS_ADDRESS (the headless shell), LAN_MOUSE_TEST_PROBE.
# Requires gnome-shell >= 45 and, for the dock tests, ubuntu-dock.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
size="${GNOME_HEADLESS_SIZE:-1280x800}"

if [[ "${1:-}" == "--test" ]]; then
    # Build on the host environment (cargo needs the real HOME), run inside.
    bin="$(cd "$repo" && cargo test -p input-emulation --test gnome_headless --no-run \
        --message-format=json 2>/dev/null |
        python3 -c 'import json,sys
for line in sys.stdin:
    m = json.loads(line)
    if m.get("reason") == "compiler-artifact" and m.get("executable") and m["target"]["name"] == "gnome_headless":
        print(m["executable"])' | tail -n1)"
    [[ -n "$bin" ]] || { echo "build of the gnome_headless test failed" >&2; exit 1; }
    set -- "$bin" --test-threads=1 --nocapture
fi
[[ $# -gt 0 ]] || { echo "usage: $0 <command> [args...] | --test" >&2; exit 2; }

command -v gnome-shell >/dev/null || { echo "gnome-shell is not installed" >&2; exit 1; }

# Short runtime dir: Wayland socket paths are limited to 108 bytes.
work="$(mktemp -d "${TMPDIR:-/tmp}/lmgs.XXXXXX")"
chmod 700 "$work"
cleanup() {
    # xdg-document-portal mounts a FUSE fs under the runtime dir; it goes away
    # with the session bus, sometimes a moment after dbus-run-session returns.
    for _ in 1 2 3 4 5; do
        fusermount3 -u "$work/run/doc" 2>/dev/null || true
        rm -rf "$work" 2>/dev/null && return
        sleep 0.2
    done
    echo "warning: could not remove $work" >&2
}
trap cleanup EXIT
mkdir -p "$work"/{run,home,config,data/gnome-shell/extensions,cache,state}
chmod 700 "$work/run"
cp -r "$repo/tests/gnome-headless/lan-mouse-test-probe@lan-mouse-custom" \
    "$work/data/gnome-shell/extensions/"

if [[ -n "${GNOME_HEADLESS_MONITORS:-}" ]]; then
    cp "$GNOME_HEADLESS_MONITORS" "$work/config/monitors.xml"
fi

# Dock like a typical Ubuntu desktop with "auto-hide" on, minus intellihide
# (with no windows open intellihide would keep the dock shown).
cat >"$work/dconf.ini" <<'EOF'
[org/gnome/shell]
enabled-extensions=['lan-mouse-test-probe@lan-mouse-custom']
disable-user-extensions=false
welcome-dialog-last-shown-version='999'

[org/gnome/mutter]
experimental-features=['scale-monitor-framebuffer', 'xwayland-native-scaling']

[org/gnome/shell/extensions/dash-to-dock]
dock-fixed=false
autohide=true
intellihide=false
require-pressure-to-show=true
pressure-threshold=100.0
dock-position='BOTTOM'
EOF

inner="$work/inner.sh"
cat >"$inner" <<'EOF'
#!/usr/bin/env bash
set -uo pipefail
dconf load / <"$WORK/dconf.ini"
monitors=()
for m in $SIZE; do monitors+=(--virtual-monitor "$m"); done
gnome-shell --headless "${monitors[@]}" --wayland-display lm-0 \
    --mode=ubuntu --no-x11 >"$WORK/shell.log" 2>&1 &
shell=$!
export WAYLAND_DISPLAY=lm-0
for _ in $(seq 1 300); do
    if [[ -f "$LAN_MOUSE_TEST_PROBE" ]] && grep -q '"startupComplete":true' "$LAN_MOUSE_TEST_PROBE"; then
        break
    fi
    kill -0 "$shell" 2>/dev/null || { echo "gnome-shell exited:"; tail -20 "$WORK/shell.log"; exit 1; }
    sleep 0.1
done
grep -q '"startupComplete":true' "$LAN_MOUSE_TEST_PROBE" 2>/dev/null ||
    { echo "gnome-shell did not finish starting:"; tail -20 "$WORK/shell.log"; kill "$shell"; exit 1; }
cp "$LAN_MOUSE_TEST_PROBE" "$WORK/probe-start.json" 2>/dev/null
"$@"
status=$?
kill "$shell" 2>/dev/null
wait "$shell" 2>/dev/null
[[ $status -eq 0 ]] || {
    echo "--- probe at start: $(cat "$WORK/probe-start.json" 2>/dev/null)"
    echo "--- gnome-shell log (tail) ---"
    grep -v St-CRITICAL "$WORK/shell.log" | tail -30
}
exit $status
EOF
chmod +x "$inner"

env -i \
    PATH="/usr/local/bin:/usr/bin:/bin" \
    HOME="$work/home" \
    LANG="${LANG:-C.UTF-8}" \
    XDG_RUNTIME_DIR="$work/run" \
    XDG_CONFIG_HOME="$work/config" \
    XDG_DATA_HOME="$work/data" \
    XDG_CACHE_HOME="$work/cache" \
    XDG_STATE_HOME="$work/state" \
    XDG_SESSION_TYPE=wayland \
    RUST_LOG="${RUST_LOG:-}" \
    RUST_BACKTRACE="${RUST_BACKTRACE:-0}" \
    WORK="$work" SIZE="$size" \
    LAN_MOUSE_TEST_PROBE="$work/run/probe.json" \
    dbus-run-session -- "$inner" "$@"
