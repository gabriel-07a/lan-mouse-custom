#!/usr/bin/env bash
# Remove Lan Mouse's per-user data on Linux and macOS.
#
# A package upgrade intentionally keeps the user's devices, permissions and
# certificates. Use this script (or the newer `lan-mouse --reset-config`) when
# you want a completely clean start - for example after installing a release
# over the leftovers of an older build.
#
# The default is reversible: every directory is renamed to a sibling
# `lan-mouse.bak-<unix timestamp>` instead of being deleted, so no old device
# is visible to the next start but the data can still be restored by renaming
# the backup back. Pass --purge to delete it permanently.
#
# This script only touches directories literally named "lan-mouse" below the
# user's config/state/cache directories (plus the audit log directory on
# macOS). It never touches the rest of $HOME, system files or package files.
#
# Usage:
#   scripts/install-clean.sh                 # show, ask, then move to backups
#   scripts/install-clean.sh --dry-run       # only list
#   scripts/install-clean.sh --yes           # no prompt (scripts/CI)
#   scripts/install-clean.sh --purge         # delete permanently (no backup)
#   scripts/install-clean.sh --no-kill       # don't stop a running lan-mouse
set -euo pipefail

app_dir_name="lan-mouse"
dry_run=0
assume_yes=0
purge=0
kill_running=1

usage() {
    cat <<'EOF'
Usage: install-clean.sh [--dry-run] [--yes] [--purge] [--no-kill]

Back up (move) Lan Mouse per-user data so the next start is a fresh install:
configuration, devices, permissions, certificate, audit log and portal consent
tokens. A package upgrade keeps this data on purpose; this script is the
explicit reset.

  --dry-run   only list what would be moved/deleted
  -y, --yes   do not ask for confirmation
  --purge     permanently delete the data instead of moving it to
              `lan-mouse.bak-<timestamp>` backups (not reversible)
  --no-kill   do not try to stop a running lan-mouse process first
  -h, --help  show this help

Restore a backup with e.g.:
  mv ~/.config/lan-mouse.bak-<timestamp> ~/.config/lan-mouse
EOF
}

for arg in "$@"; do
    case "$arg" in
        --dry-run) dry_run=1 ;;
        -y | --yes) assume_yes=1 ;;
        --purge) purge=1 ;;
        --no-kill) kill_running=0 ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "install-clean.sh: unknown argument: $arg" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [ -z "${HOME:-}" ] || [ "$HOME" = "/" ]; then
    echo "install-clean.sh: refusing to run without a sane HOME" >&2
    exit 2
fi

config_base="${XDG_CONFIG_HOME:-$HOME/.config}"
state_base="${XDG_STATE_HOME:-$HOME/.local/state}"
cache_base="${XDG_CACHE_HOME:-$HOME/.cache}"

paths=(
    "$config_base/$app_dir_name"
    "$state_base/$app_dir_name"
    "$cache_base/$app_dir_name"
)
case "$(uname -s 2>/dev/null || echo unknown)" in
    Darwin) paths+=("$HOME/Library/Logs/$app_dir_name") ;;
esac

# Validate every path before touching anything: absolute, final component
# exactly "lan-mouse", not directly below the filesystem root, no "." / "..".
validated=()
for path in "${paths[@]}"; do
    case "$path" in
        /*) ;;
        *)
            echo "install-clean.sh: refusing non-absolute path: $path" >&2
            exit 1
            ;;
    esac
    case "/$path/" in
        */../* | */.//*)
            echo "install-clean.sh: refusing path with '.' or '..': $path" >&2
            exit 1
            ;;
    esac
    if [ "${path##*/}" != "$app_dir_name" ]; then
        echo "install-clean.sh: refusing unexpected directory: $path" >&2
        exit 1
    fi
    parent="${path%/*}"
    if [ -z "$parent" ] || [ "$parent" = "/" ]; then
        echo "install-clean.sh: refusing path directly below the filesystem root: $path" >&2
        exit 1
    fi
    validated+=("$path")
done

if [ "$kill_running" -eq 1 ] && pgrep -x "$app_dir_name" >/dev/null 2>&1; then
    if [ "$dry_run" -eq 1 ]; then
        echo "Lan Mouse is running; it would be stopped first (--no-kill skips this)."
    else
        echo "Stopping the running lan-mouse process ..."
        pkill -x "$app_dir_name" 2>/dev/null || true
        sleep 1
        if pgrep -x "$app_dir_name" >/dev/null 2>&1; then
            echo "install-clean.sh: lan-mouse is still running; quit it first or pass --no-kill" >&2
            exit 1
        fi
    fi
fi

if [ "$purge" -eq 1 ]; then
    echo "Lan Mouse per-user directories (will be deleted permanently with --purge):"
else
    echo "Lan Mouse per-user directories (will be moved to \`$app_dir_name.bak-<timestamp>\` backups):"
fi
found=0
for path in "${validated[@]}"; do
    if [ -e "$path" ] || [ -L "$path" ]; then
        echo "  $path"
        found=1
    else
        echo "  $path (not present)"
    fi
done

if [ "$found" -eq 0 ]; then
    echo "Nothing to do."
    exit 0
fi

if [ "$dry_run" -eq 1 ]; then
    echo "Dry run: nothing was moved or deleted."
    exit 0
fi

if [ "$assume_yes" -eq 0 ]; then
    if [ ! -t 0 ]; then
        echo "install-clean.sh: refusing without confirmation (pass --yes)" >&2
        exit 2
    fi
    if [ "$purge" -eq 1 ]; then
        printf 'Permanently delete these directories? This cannot be undone. [y/N] '
        read -r answer || answer=""
        case "$answer" in
            y | Y | yes | YES) ;;
            *)
                echo "Aborted, nothing was deleted."
                exit 1
                ;;
        esac
    else
        printf 'Move these directories to backups? [Y/n] '
        read -r answer || answer=""
        case "$answer" in
            "" | y | Y | yes | YES) ;;
            *)
                echo "Aborted, nothing was moved."
                exit 1
                ;;
        esac
    fi
fi

# One timestamp for the whole run so all backups of this reset sort together.
# The numeric suffix keeps repeated resets within the same second unique.
stamp="$(date -u +%s)"
for path in "${validated[@]}"; do
    if [ ! -e "$path" ] && [ ! -L "$path" ]; then
        continue
    fi
    if [ "$purge" -eq 1 ]; then
        # rm does not follow a symlink argument: only the link itself is
        # removed, never a target outside the app's own directory.
        rm -rf -- "$path"
        echo "permanently deleted $path"
        continue
    fi
    backup="${path}.bak-${stamp}"
    suffix=1
    while [ -e "$backup" ] || [ -L "$backup" ]; do
        backup="${path}.bak-${stamp}-${suffix}"
        suffix=$((suffix + 1))
    done
    # Renaming within the same parent directory is atomic and reversible;
    # a symlink is moved as a link and its target is never followed.
    mv -- "$path" "$backup"
    echo "moved $path -> $backup"
done

echo "Done. The next Lan Mouse start is a fresh install."
if [ "$purge" -eq 1 ]; then
    echo "Note: the data was deleted permanently (--purge); it cannot be restored."
else
    echo "Restore a backup by renaming it back, e.g.:"
    echo "  mv <dir>.bak-<timestamp> <dir>"
fi
case "$(uname -s 2>/dev/null || echo unknown)" in
    Darwin)
        echo "Note: macOS accessibility/input-monitoring permissions are managed by the OS;"
        echo "      reset them in System Settings if you want a completely clean state."
        ;;
    *)
        echo "Note: the compositor will ask for portal permissions again on the next start."
        ;;
esac
