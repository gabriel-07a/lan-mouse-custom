#!/usr/bin/env bash
# QA harness: bidirectional hand-off verification (independent, task-11).
#
# Verifies the two-way hand-off churn fix for:
#   * Ubuntu source (current main) -> Windows 0.2.18 receiver: `LeaveAt` every
#     ~1-2 s while crossing;
#   * Windows 0.2.18 source -> Ubuntu receiver: repeated `handoff: return
#     request ...` (`along=Some` edge returns and `along=None` EnterOnly
#     re-entry loops) while Windows is still controlling.
#
# Cause analysis (task-9): the source guard was time-only (fixed 200 ms) and
# the receiver-side EnterOnly capture re-entry loop existed on both builds, so
# a same-build 0.2.19 alone is not enough. The candidate must be validated on
# both sides with the position-aware source filter + receiver re-entry gate.
#
# Legs:
#   1. independent replay harness (tests/qa_handoff_bidirectional.rs): the
#      shipping `src/capture.rs` CaptureTask is driven in both directions and
#      its forwarded deltas feed independent current/0.2.18 receiver models;
#   2. same-build candidate validation: version metadata, Ubuntu local
#      release build, Windows cross-compile and packaging/installer checks;
#   3. required repo gates: fmt, workspace tests, clippy -D warnings, and the
#      previous hand-off/shutdown/clipboard regression harnesses.
#
# Usage:
#   scripts/qa-handoff-bidirectional.sh                  # all checks
#   scripts/qa-handoff-bidirectional.sh replay           # only leg 1
#   scripts/qa-handoff-bidirectional.sh candidate        # only leg 2
#   scripts/qa-handoff-bidirectional.sh gates            # only leg 3
#   QA_SKIP_RELEASE=1   skip the (slow) Ubuntu release build
#   QA_SKIP_WINDOWS=1   skip the Windows cross-compile
#   QA_SKIP_PREVIOUS=1  skip the previous full hand-off gate script
#   QA_EXPECT_VERSION=0.2.19  candidate version (default 0.2.19)
#
# The daemon QA legs bind a Unix socket under <QA_OUT>/.../runtime; keep QA_OUT
# short (the default target/qa-bidi is sized for the 108-byte SUN_LEN limit on
# this repo path). The previous full gate script is run with a 1-letter dir.
#
# No publish, no push, no CI: local validation only.
# Exit code: number of failed checks (0 = pass).

set -u

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

OUT="${QA_OUT:-$REPO_ROOT/target/qa-bidi}"
LOG="$OUT/logs"
ZIG="${QA_ZIG:-$REPO_ROOT/.tools/zig-x86_64-linux-0.15.2/zig}"
EXPECT_VERSION="${QA_EXPECT_VERSION:-0.2.19}"
SKIP_RELEASE="${QA_SKIP_RELEASE:-0}"
SKIP_WINDOWS="${QA_SKIP_WINDOWS:-0}"
SKIP_PREVIOUS="${QA_SKIP_PREVIOUS:-0}"

CHECKS=0
FAILS=0
SKIPS=0

say() { printf '%s\n' "$*"; }
note() { printf '  [info] %s\n' "$*"; }
pass() { CHECKS=$((CHECKS + 1)); printf '  [PASS] %s\n' "$*"; }
fail() { CHECKS=$((CHECKS + 1)); FAILS=$((FAILS + 1)); printf '  [FAIL] %s\n' "$*"; }
skip() { CHECKS=$((CHECKS + 1)); SKIPS=$((SKIPS + 1)); printf '  [SKIP] %s\n' "$*"; }

mkdir -p "$LOG"

fingerprint() {
    {
        echo "date: $(date -Is)"
        echo "head: $(git rev-parse HEAD 2>/dev/null || echo unknown)"
        echo "head-subject: $(git log -1 --format=%s 2>/dev/null || echo unknown)"
        echo "crate-version: $(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
        echo "status:"
        git status --porcelain=v1 2>/dev/null || true
        echo "hashes:"
        for f in src/capture.rs input-emulation/src/libei.rs src/service.rs \
                 lan-mouse-proto/src/lib.rs \
                 tests/qa_handoff_bidirectional.rs \
                 tests/qa_handoff_old_receiver.rs \
                 tests/qa_handoff_old_receiver_verify.rs \
                 scripts/qa-handoff-bidirectional.sh \
                 docs/testing/handoff-bidirectional-verification.md; do
            [ -f "$f" ] && sha256sum "$f"
        done
    } | tee "$OUT/tree.txt"
}

run_gate() {
    local label="$1"
    shift
    local log="$LOG/$(printf '%s' "$label" | tr ' /:' '___').log"
    note "$label"
    if "$@" >"$log" 2>&1; then
        pass "$label"
        return 0
    fi
    fail "$label (log: $log)"
    tail -6 "$log" | sed 's/^/       /'
    return 1
}

# --- leg 1: independent replay harness --------------------------------------

replay_checks() {
    say ""
    say "== 1. Independent bidirectional replay (shipping capture task) =="
    run_gate "replay: shipped src/capture.rs vs current/0.2.18 receiver models" \
        cargo test --test qa_handoff_bidirectional
}

# --- leg 2: same-build candidate validation --------------------------------

zig_wrappers() {
    mkdir -p "$OUT/zig/global" "$OUT/zig/cache"
    cat > "$OUT/zig/cc" <<EOF
#!/bin/bash
# cc-rs passes --target=x86_64-pc-windows-gnu; zig 0.15 wants
# x86_64-windows-gnu, and the host ~/.cache/zig is read-only in this sandbox.
export ZIG_GLOBAL_CACHE_DIR="$OUT/zig/global"
export ZIG_LOCAL_CACHE_DIR="$OUT/zig/cache"
args=()
skip_next=0
for a in "\$@"; do
  if [ "\$skip_next" = 1 ]; then skip_next=0; continue; fi
  case "\$a" in
    --target=*|-target=*) continue ;;
    --target|-target) skip_next=1; continue ;;
  esac
  args+=("\$a")
done
exec "$ZIG" cc -target x86_64-windows-gnu "\${args[@]}"
EOF
    cat > "$OUT/zig/ar" <<EOF
#!/bin/sh
exec "$ZIG" ar "\$@"
EOF
    chmod +x "$OUT/zig/cc" "$OUT/zig/ar"
}

candidate_checks() {
    say ""
    say "== 2. Same-build candidate validation (version $EXPECT_VERSION) =="
    local version
    version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
    if [ "$version" = "$EXPECT_VERSION" ]; then
        pass "candidate version metadata: $version"
    else
        skip "candidate version metadata: source is $version (expected $EXPECT_VERSION; version bump is a release-owner action — see docs §5.1)"
    fi

    if [ "$SKIP_RELEASE" = "1" ]; then
        skip "Ubuntu local release build (QA_SKIP_RELEASE=1)"
    else
        run_gate "Ubuntu local release build (lan-mouse, headless backends)" \
            cargo build -p lan-mouse --release --no-default-features
        if [ -x target/release/lan-mouse ]; then
            local banner
            banner="$(target/release/lan-mouse --version 2>&1 | tr '\n' ' ')"
            note "release binary: $banner"
        fi
    fi

    if [ "$SKIP_WINDOWS" = "1" ]; then
        skip "Windows cross-compile (QA_SKIP_WINDOWS=1)"
    elif [ ! -x "$ZIG" ]; then
        skip "Windows cross-compile (vendored zig not found at $ZIG)"
    elif ! rustup target list --installed 2>/dev/null | grep -qx x86_64-pc-windows-gnu; then
        skip "Windows cross-compile (x86_64-pc-windows-gnu target not installed)"
    else
        zig_wrappers
        run_gate "Windows cross-compile (all targets, zig wrapper)" \
            env CC_x86_64_pc_windows_gnu="$OUT/zig/cc" \
                AR_x86_64_pc_windows_gnu="$OUT/zig/ar" \
                CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$OUT/zig/cc" \
                cargo check --workspace --all-targets --exclude lan-mouse-gtk \
                --target x86_64-pc-windows-gnu
    fi

    run_gate "installer/packaging checks (same-build artifacts)" \
        cargo test --test packaging
}

# --- leg 3: repo gates + previous hand-off regressions ----------------------

gates() {
    say ""
    say "== 3. Repo gates and previous hand-off regressions =="
    run_gate "cargo fmt --check" cargo fmt --check
    run_gate "cargo test -p lan-mouse --lib" cargo test -p lan-mouse --lib
    run_gate "cargo test -p input-emulation --lib (receiver deadband/settle)" \
        cargo test -p input-emulation --lib
    run_gate "cargo test --test qa_handoff_old_receiver_verify (previous verifier gate)" \
        cargo test --test qa_handoff_old_receiver_verify
    run_gate "cargo test --test qa_handoff_old_receiver (task-5 harness)" \
        cargo test --test qa_handoff_old_receiver
    run_gate "cargo test --test qa_handoff_shutdown (shutdown contract)" \
        cargo test --test qa_handoff_shutdown
    run_gate "cargo test --test qa_clipboard_atomicity (clipboard regression)" \
        cargo test --test qa_clipboard_atomicity
    run_gate "cargo test --test qa_connection_state (connect state regression)" \
        cargo test --test qa_connection_state
    run_gate "cargo test --workspace --all-features" \
        cargo test --workspace --all-features
    run_gate "cargo clippy --workspace --all-targets --all-features -- -D warnings" \
        cargo clippy --workspace --all-targets --all-features -- -D warnings
    run_gate "scripts/qa-handoff-shutdown.sh (daemon cursor lifecycle)" \
        env QA_BASE_DIR="$OUT/daemon" scripts/qa-handoff-shutdown.sh
    if [ "$SKIP_PREVIOUS" = "1" ]; then
        skip "scripts/qa-handoff-old-receiver.sh (QA_SKIP_PREVIOUS=1)"
    elif [ -x scripts/qa-handoff-old-receiver.sh ]; then
        # Keep the nested work dir short: the daemon's Unix socket lives under
        # <abs OUT>/.../runtime and must stay under the 108-byte SUN_LEN limit.
        run_gate "scripts/qa-handoff-old-receiver.sh (previous full gate)" \
            env QA_OUT="$OUT/p" QA_KEEP=1 scripts/qa-handoff-old-receiver.sh all
    else
        skip "scripts/qa-handoff-old-receiver.sh (not executable/absent)"
    fi
}

# --- main -------------------------------------------------------------------

MODE="${1:-all}"
say "== bidirectional hand-off verification =="
say "repo: $REPO_ROOT"
say "output: $OUT"
mkdir -p "$OUT"
fingerprint

case "$MODE" in
    all) replay_checks; candidate_checks; gates ;;
    replay) replay_checks ;;
    candidate) candidate_checks ;;
    gates) gates ;;
    *) say "unknown mode '$MODE' (expected: all|replay|candidate|gates)"; exit 2 ;;
esac

say ""
say "== summary: $CHECKS checks, $((CHECKS - FAILS - SKIPS)) passed, $FAILS failed, $SKIPS skipped =="
note "logs kept under $LOG"
exit "$FAILS"
