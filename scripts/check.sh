#!/usr/bin/env bash
# Run the checks CI runs, locally, without stopping at the first failure.
#
# `cargo test` stops at the first test target that fails, so a red target
# hides every later one; CI then reports failures that never showed up here.
# This script runs every target (`--no-fail-fast`, or nextest when installed)
# and reports all failing steps at the end.
#
# Usage:
#   scripts/check.sh              # fmt, clippy -D warnings, all tests (Linux)
#   scripts/check.sh --windows    # also clippy -D warnings for the Windows target
#   scripts/check.sh --keep       # keep target/ afterwards (default: cargo clean)
#
# --windows cross-lints x86_64-pc-windows-gnu with the Zig toolchain under
# .tools/ (`.tools/zig-*/zig`) as C compiler/linker; it needs
# `rustup target add x86_64-pc-windows-gnu`. It catches Windows-only lints
# (dead code behind `cfg(unix)`, unused imports) before CI does.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

windows=0
keep=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --windows) windows=1; shift ;;
        --keep) keep=1; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

failed=()
step() {
    local name="$1"; shift
    echo "=== $name ==="
    if ! "$@"; then
        failed+=("$name")
    fi
}

step "fmt" cargo fmt --all --check
step "clippy (linux)" cargo clippy --workspace --all-targets --all-features -- -D warnings

if command -v cargo-nextest >/dev/null 2>&1; then
    step "tests (nextest)" cargo nextest run --workspace --all-features --no-fail-fast
    # nextest does not run doctests
    step "doctests" cargo test --workspace --all-features --doc
else
    step "tests" cargo test --workspace --all-features --no-fail-fast
fi

if [[ $windows == 1 ]]; then
    zig="$(ls -d "$PWD"/.tools/zig-*/zig 2>/dev/null | head -n1)"
    if [[ -z "$zig" ]]; then
        echo "no Zig under .tools/; download one to .tools/zig-<arch>-<os>-<version>/" >&2
        failed+=("clippy (windows): no zig")
    else
        wrap="$PWD/target/zig-wrap"
        mkdir -p "$wrap/cache" "$wrap/global"
        # zig rejects the `--target=` rustc/cc-rs pass along; it gets its own.
        cat >"$wrap/cc" <<EOF
#!/usr/bin/env bash
export ZIG_GLOBAL_CACHE_DIR="$wrap/global" ZIG_LOCAL_CACHE_DIR="$wrap/cache"
args=(); skip=0
for a in "\$@"; do
  if [[ \$skip == 1 ]]; then skip=0; continue; fi
  case "\$a" in --target=*|-target=*) continue ;; --target|-target) skip=1; continue ;; esac
  args+=("\$a")
done
exec "$zig" cc -target x86_64-windows-gnu "\${args[@]}"
EOF
        printf '#!/bin/sh\nexec "%s" ar "$@"\n' "$zig" >"$wrap/ar"
        chmod +x "$wrap/cc" "$wrap/ar"
        step "clippy (windows)" env RUSTFLAGS="-D warnings" \
            CC_x86_64_pc_windows_gnu="$wrap/cc" \
            AR_x86_64_pc_windows_gnu="$wrap/ar" \
            CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$wrap/cc" \
            cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu
    fi
fi

if [[ $keep == 0 ]]; then
    # AGENTS.md disk hygiene: never leave a build behind.
    cargo clean --quiet
fi

if [[ ${#failed[@]} -gt 0 ]]; then
    echo
    echo "FAILED: ${failed[*]}"
    exit 1
fi
echo
echo "all checks passed"
