#!/usr/bin/env bash
# QA harness: old-Windows-receiver hand-off verification (independent).
#
# Verifies the fix for: "hand-off Ubuntu touchpad -> Windows still churns
# LeaveAt because the source keeps forwarding the post-forward touchpad
# backswing; the old Windows 0.2.18 receiver (RETURN_MARGIN_PX = 3.0, exact
# EnterAt edge anchor) hands control back and the Ubuntu cursor stays visible".
#
# The harness has three independent legs:
#
#   1. dynamic replay of the shipping `src/capture.rs` (read-only `include!`
#      into `tests/qa_handoff_old_receiver_verify.rs`, real `CaptureTask`
#      driven with trace-shaped deltas) against an independently written
#      Windows 0.2.18 receiver model;
#   2. a pre-fix sensitivity check: the same harness compiled against the
#      pinned pre-fix commit (`QA_PRE_FIX_REF`, default 8b6981d) must FAIL and
#      show the leaked -17 px backswing, proving the test detects the bug;
#   3. the required repo gates: fmt, workspace tests, clippy -D warnings, the
#      Windows cross-compile with the repo-vendored zig wrapper, the existing
#      `tests/qa_handoff_shutdown.rs` and `scripts/qa-handoff-shutdown.sh`.
#
# Usage:
#   scripts/qa-handoff-old-receiver.sh                 # all checks
#   scripts/qa-handoff-old-receiver.sh gates           # only repo gates
#   scripts/qa-handoff-old-receiver.sh replay          # only replay+pre-fix
#   QA_SKIP_WINDOWS=1 scripts/qa-handoff-old-receiver.sh
#   QA_SKIP_PREFIX=1  scripts/qa-handoff-old-receiver.sh
#   QA_KEEP=1         scripts/qa-handoff-old-receiver.sh   # keep work dirs
#
# Requirements: bash, coreutils, rust toolchain, x86_64-pc-windows-gnu target
# and the vendored zig (for the Windows gate). Local-only: no network service,
# no push/CI/release. Exit code: number of failed checks (0 = pass).

set -u

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

OUT="${QA_OUT:-$REPO_ROOT/target/qa-old-receiver}"
LOG="$OUT/logs"
PREFIX_REF="${QA_PRE_FIX_REF:-8b6981d}"
KEEP="${QA_KEEP:-0}"
SKIP_WINDOWS="${QA_SKIP_WINDOWS:-0}"
SKIP_PREFIX="${QA_SKIP_PREFIX:-0}"
ZIG="${QA_ZIG:-$REPO_ROOT/.tools/zig-x86_64-linux-0.15.2/zig}"

CHECKS=0
FAILS=0
SKIPS=0

say() { printf '%s\n' "$*"; }
note() { printf '  [info] %s\n' "$*"; }
pass() { CHECKS=$((CHECKS + 1)); printf '  [PASS] %s\n' "$*"; }
fail() { CHECKS=$((CHECKS + 1)); FAILS=$((FAILS + 1)); printf '  [FAIL] %s\n' "$*"; }
skip() { CHECKS=$((CHECKS + 1)); SKIPS=$((SKIPS + 1)); printf '  [SKIP] %s\n' "$*"; }

mkdir -p "$LOG"

# --- tree fingerprint -------------------------------------------------------
# Every gate result below must describe one immutable tree. Record what was
# tested so the evidence in docs/testing/handoff-old-receiver-verification.md
# can be matched to the checkout.
fingerprint() {
    {
        echo "date: $(date -Is)"
        echo "head: $(git rev-parse HEAD 2>/dev/null || echo unknown)"
        echo "head-subject: $(git log -1 --format=%s 2>/dev/null || echo unknown)"
        echo "status:"
        git status --porcelain=v1 2>/dev/null || true
        echo "hashes:"
        for f in src/capture.rs tests/qa_handoff_old_receiver_verify.rs \
                 tests/qa_handoff_old_receiver.rs \
                 docs/testing/handoff-old-receiver-verification.md; do
            [ -f "$f" ] && sha256sum "$f"
        done
    } | tee "$OUT/tree.txt"
}

# --- gate helper ------------------------------------------------------------
# run_gate <label> <command...>: runs the command, captures the log, counts
# one PASS/FAIL. Returns the command status so callers can chain.
run_gate() {
    local label="$1"
    shift
    local log="$LOG/$(printf '%s' "$label" | tr ' /' '__').log"
    note "$label"
    if "$@" >"$log" 2>&1; then
        pass "$label"
        return 0
    fi
    fail "$label (log: $log)"
    tail -5 "$log" | sed 's/^/       /'
    return 1
}

# --- 1. dynamic replay + pre-fix sensitivity --------------------------------

replay_checks() {
    say ""
    say "== 1. Dynamic replay of the shipping capture task =="
    run_gate "replay: shipping src/capture.rs vs Windows 0.2.18 receiver model" \
        cargo test --test qa_handoff_old_receiver_verify

    if [ -f tests/qa_handoff_old_receiver.rs ]; then
        run_gate "replay: task-5 old-receiver harness" \
            cargo test --test qa_handoff_old_receiver
    else
        skip "replay: task-5 harness (tests/qa_handoff_old_receiver.rs absent)"
    fi

    if [ "$SKIP_PREFIX" = "1" ]; then
        skip "sensitivity: pre-fix build must fail on the leaked backswing (QA_SKIP_PREFIX=1)"
        return
    fi

    say "  preparing pre-fix tree (git archive $PREFIX_REF)..."
    local prefix="$OUT/prefix-tree"
    rm -rf "$prefix"
    mkdir -p "$prefix" "$prefix/tests"
    if ! git archive "$PREFIX_REF" 2>"$LOG/prefix-archive.log" | tar -x -C "$prefix" 2>>"$LOG/prefix-archive.log"; then
        fail "sensitivity: cannot materialize pre-fix tree $PREFIX_REF"
        return
    fi
    cp tests/qa_handoff_old_receiver_verify.rs "$prefix/tests/"
    # Adapt the harness to the pre-fix API: the pre-fix (8b6981d)
    # CaptureTask has none of the later guard fields (residual budget, fixed
    # window, position-aware band/deadline, remote-entry dwell) and its guard
    # disarms on the first forward delta. Only the API differences are patched;
    # the trace and both state machines (production source and old-receiver
    # model) are unchanged.
    python3 - "$prefix/tests/qa_handoff_old_receiver_verify.rs" <<'PY'
import sys

path = sys.argv[1]
s = open(path).read()
for field in [
    "            reverse_guard_until: None,\n",
    "            handoff_inward_px: None,\n",
    "            handoff_leading_until: None,\n",
    "            remote_peer_since: None,\n",
    "            enter_only_take_back_sent: false,\n",
]:
    s = s.replace(field, "")
start = s.index("    fn replay_expire_reverse_guard(")
end = s.index("\n    }\n", start) + len("\n    }\n")
s = s[:start] + (
    "    fn replay_expire_reverse_guard(_task: &mut CaptureTask) {\n"
    "        // pre-fix tree: the guard disarms on the first forward delta\n"
    "    }\n"
) + s[end:]
open(path, "w").write(s)
PY
    local prefix_log="$LOG/prefix-replay.log"
    local filtered
    # The bug: pre-fix forwards the post-forward -17 px backswing.
    ( cd "$prefix" && CARGO_TARGET_DIR="$REPO_ROOT/target" \
        cargo test --test qa_handoff_old_receiver_verify \
        capture::the_post_forward_backswing_is_suppressed_and_the_old_receiver_never_returns \
        -- --exact ) >"$prefix_log" 2>&1
    local bug_status=$?
    # The first forward delta was already immediate before the fix.
    ( cd "$prefix" && CARGO_TARGET_DIR="$REPO_ROOT/target" \
        cargo test --test qa_handoff_old_receiver_verify \
        capture::the_first_forward_delta_is_forwarded_immediately_with_its_exact_value \
        -- --exact ) >>"$prefix_log" 2>&1
    local fwd_status=$?

    filtered="$(cat "$prefix_log")"
    if [ "$bug_status" -ne 0 ] && printf '%s' "$filtered" | grep -q -- "-17.0"; then
        pass "sensitivity: pre-fix tree leaks the -17 px backswing (replay fails as expected)"
    else
        fail "sensitivity: pre-fix tree did not reproduce the leak (log: $prefix_log)"
    fi
    if [ "$fwd_status" -eq 0 ]; then
        pass "sensitivity: first forward delta was immediate before and after the fix"
    else
        fail "sensitivity: first forward delta FAILS on the pre-fix tree (broken baseline; log: $prefix_log)"
    fi
    [ "$KEEP" = "1" ] || rm -rf "$prefix"
}

# --- 2. repo gates ----------------------------------------------------------

windows_prepare() {
    mkdir -p "$OUT/zig"
    local global="$OUT/zig/global"
    local local_cache="$OUT/zig/cache"
    mkdir -p "$global" "$local_cache"
    cat > "$OUT/zig/cc" <<EOF
#!/bin/bash
# cc-rs passes --target=x86_64-pc-windows-gnu; zig 0.15 wants
# x86_64-windows-gnu, and the host ~/.cache/zig is read-only in this sandbox.
export ZIG_GLOBAL_CACHE_DIR="$global"
export ZIG_LOCAL_CACHE_DIR="$local_cache"
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

gates() {
    say ""
    say "== 2. Repo gates =="
    run_gate "cargo fmt --check" cargo fmt --check
    run_gate "cargo test -p lan-mouse --lib (capture guard unit tests)" \
        cargo test -p lan-mouse --lib
    run_gate "cargo test --test qa_handoff_shutdown (existing lifecycle contract)" \
        cargo test --test qa_handoff_shutdown
    run_gate "cargo test --test qa_clipboard_atomicity (clipboard regression audit)" \
        cargo test --test qa_clipboard_atomicity
    run_gate "cargo test --test qa_connection_state (connect/disconnect regression audit)" \
        cargo test --test qa_connection_state
    run_gate "cargo test --workspace --all-features" \
        cargo test --workspace --all-features
    run_gate "cargo clippy --workspace --all-targets --all-features -- -D warnings" \
        cargo clippy --workspace --all-targets --all-features -- -D warnings

    if [ "$SKIP_WINDOWS" = "1" ]; then
        skip "Windows cross-check (QA_SKIP_WINDOWS=1)"
    elif [ ! -x "$ZIG" ]; then
        skip "Windows cross-check (vendored zig not found at $ZIG)"
    elif ! rustup target list --installed 2>/dev/null | grep -qx x86_64-pc-windows-gnu; then
        skip "Windows cross-check (x86_64-pc-windows-gnu target not installed)"
    else
        windows_prepare
        run_gate "Windows cross-check (x86_64-pc-windows-gnu, zig wrapper)" \
            env CC_x86_64_pc_windows_gnu="$OUT/zig/cc" \
                AR_x86_64_pc_windows_gnu="$OUT/zig/ar" \
                CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$OUT/zig/cc" \
                cargo check --workspace --all-targets \
                --target x86_64-pc-windows-gnu
    fi
}

# --- 3. real daemon shutdown lifecycle --------------------------------------

lifecycle() {
    say ""
    say "== 3. Daemon shutdown cursor lifecycle (existing black-box QA) =="
    if [ -x scripts/qa-handoff-shutdown.sh ]; then
        run_gate "scripts/qa-handoff-shutdown.sh (SIGTERM/cli shutdown/stale markers)" \
            env QA_BASE_DIR="$OUT/daemon" scripts/qa-handoff-shutdown.sh
    else
        skip "scripts/qa-handoff-shutdown.sh (not executable/absent)"
    fi
}

# --- main -------------------------------------------------------------------

MODES="${1:-all}"
say "== hand-off old-receiver verification =="
say "repo: $REPO_ROOT"
say "output: $OUT"
mkdir -p "$OUT"
fingerprint

case "$MODES" in
    all) replay_checks; gates; lifecycle ;;
    replay) replay_checks ;;
    gates) gates ;;
    lifecycle) lifecycle ;;
    *) say "unknown mode '$MODES' (expected: all|replay|gates|lifecycle)"; exit 2 ;;
esac

say ""
say "== summary: $CHECKS checks, $((CHECKS - FAILS - SKIPS)) passed, $FAILS failed, $SKIPS skipped =="
if [ "$KEEP" != "1" ]; then
    note "logs kept under $LOG (git-ignored); QA_KEEP=1 keeps the prefix tree too"
fi
exit "$FAILS"
