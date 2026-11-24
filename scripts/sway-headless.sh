#!/usr/bin/env bash
# Run a command inside a throwaway headless sway (wlroots), with its own
# runtime dir and no input devices; nothing touches the user's desktop.
#
# Usage:
#   scripts/sway-headless.sh <command> [args...]
#   scripts/sway-headless.sh --test    # build and run tests/wlroots_headless.rs
#
# SWAY: the sway binary (default: `sway` on PATH). It can be an unpacked
# package, e.g. `apt-get download sway libwlroots-0.19 ...` + `dpkg-deb -x`,
# with SWAY_LD_LIBRARY_PATH pointing at its libraries.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sway="${SWAY:-$(command -v sway || true)}"
[[ -x "$sway" ]] || { echo "sway is not installed (set SWAY=/path/to/sway)" >&2; exit 1; }

if [[ "${1:-}" == "--test" ]]; then
    bin="$(cd "$repo" && cargo test --test wlroots_headless --no-run \
        --message-format=json 2>/dev/null |
        python3 -c 'import json,sys
for line in sys.stdin:
    m = json.loads(line)
    if m.get("reason") == "compiler-artifact" and m.get("executable") and m["target"]["name"] == "wlroots_headless":
        print(m["executable"])' | tail -n1)"
    [[ -n "$bin" ]] || { echo "build of the wlroots_headless test failed" >&2; exit 1; }
    set -- "$bin" --test-threads=1 --nocapture
fi
[[ $# -gt 0 ]] || { echo "usage: $0 <command> [args...] | --test" >&2; exit 2; }

# Short runtime dir: Wayland socket paths are limited to 108 bytes.
work="$(mktemp -d "${TMPDIR:-/tmp}/lmsw.XXXXXX")"
chmod 700 "$work"
trap 'rm -rf "$work"' EXIT
printf 'output HEADLESS-1 resolution 1280x800\n' >"$work/config"

env -i PATH="/usr/local/bin:/usr/bin:/bin" HOME="$work" XDG_RUNTIME_DIR="$work" \
    LD_LIBRARY_PATH="${SWAY_LD_LIBRARY_PATH:-}" \
    WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman \
    "$sway" -c "$work/config" >"$work/sway.log" 2>&1 &
sway_pid=$!

socket=""
for _ in $(seq 1 100); do
    socket="$(cd "$work" && ls wayland-* 2>/dev/null | grep -v lock | head -n1 || true)"
    [[ -n "$socket" ]] && break
    kill -0 "$sway_pid" 2>/dev/null || { echo "sway exited:"; cat "$work/sway.log"; exit 1; }
    sleep 0.1
done
[[ -n "$socket" ]] || { echo "sway did not start:"; cat "$work/sway.log"; kill "$sway_pid"; exit 1; }

set +e
env -i PATH="/usr/local/bin:/usr/bin:/bin" HOME="$HOME" XDG_RUNTIME_DIR="$work" \
    WAYLAND_DISPLAY="$socket" XDG_SESSION_TYPE=wayland \
    RUST_LOG="${RUST_LOG:-}" LAN_MOUSE_WLROOTS_HEADLESS=1 "$@"
status=$?
set -e
kill "$sway_pid" 2>/dev/null
wait "$sway_pid" 2>/dev/null
[[ $status -eq 0 ]] || { echo "--- sway log (tail) ---"; tail -20 "$work/sway.log"; }
exit $status
