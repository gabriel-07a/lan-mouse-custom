#!/usr/bin/env bash
# QA harness: hand-off / shutdown cursor lifecycle of the real headless daemon.
#
# Black-box regression test for the two reported regressions of this cycle:
#   * closing Lan Mouse left the Ubuntu/GNOME cursor hidden or parked;
#   * a stale daemon/portal session could keep hiding the pointer.
#
# The daemon is run with the dummy capture/emulation backends (no display, no
# portal, no real input devices) in an isolated XDG environment. The only
# observable contract needed is the one the bundled GNOME extension consumes:
#
#   $XDG_RUNTIME_DIR/lan-mouse/cursor-hide   fresh heartbeat => hide
#   $XDG_RUNTIME_DIR/lan-mouse/cursor-park   marker        => hide at the edge
#
# Scenarios:
#   1. stale markers left by a crashed daemon are removed on the next start
#   2. SIGTERM (systemd stop / desktop logout) exits cleanly and leaves no
#      marker behind
#   3. `lan-mouse cli shutdown` (explicit quit) exits cleanly and leaves no
#      marker behind
#   4. SIGKILL cannot clean up (documented crash window); the next start does
#   5. an idle daemon never publishes a hide heartbeat
#
# The real GUI close path (frontend window -> service SIGINT -> libei/portal
# release) and the actual GNOME Mutter cursor are hardware/manual: see
# docs/testing/handoff-and-shutdown-verification.md.
#
# Usage:
#   scripts/qa-handoff-shutdown.sh                  # all scenarios
#   scripts/qa-handoff-shutdown.sh stale_start      # selected scenarios
#   QA_KEEP=1 scripts/qa-handoff-shutdown.sh        # keep dirs/logs
#   QA_SKIP_BUILD=1 scripts/qa-handoff-shutdown.sh  # reuse target/debug/lan-mouse
#   QA_PORT=43421 scripts/qa-handoff-shutdown.sh    # custom UDP port
#
# Requirements: bash, coreutils, Linux, IPv4 loopback. The binary is built
# headless (no GTK/GPUI): cargo build -p lan-mouse --no-default-features.
# Exit code: number of failed checks (0 = pass).

set -u

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

BIN="${QA_BIN:-$REPO_ROOT/target/debug/lan-mouse}"
BASE="${QA_BASE_DIR:-$REPO_ROOT/target/qa-handoff-shutdown}"
PORT="${QA_PORT:-$((43000 + ($$ % 500)))}"
# Discovery defaults to 4245; pick another port so the harness never collides
# with an already running Lan Mouse instance on the test host.
DISCOVERY_PORT="${QA_DISCOVERY_PORT:-$((PORT + 1000))}"
KEEP="${QA_KEEP:-0}"
SKIP_BUILD="${QA_SKIP_BUILD:-0}"

CHECKS=0
FAILS=0
DAEMON_PID=""
D=""

say() { printf '%s\n' "$*"; }
note() { printf '  [info] %s\n' "$*"; }
pass() { CHECKS=$((CHECKS + 1)); printf '  [PASS] %s\n' "$*"; }
fail() { CHECKS=$((CHECKS + 1)); FAILS=$((FAILS + 1)); printf '  [FAIL] %s\n' "$*"; }

check_absent() {
    local name="$1" path="$2"
    if [ ! -e "$path" ]; then
        pass "$name"
    else
        fail "$name (ficheiro ainda existe: $path)"
    fi
}

check_present() {
    local name="$1" path="$2"
    if [ -e "$path" ]; then
        pass "$name"
    else
        fail "$name (ficheiro não existe: $path)"
    fi
}

# --- instance plumbing ------------------------------------------------------

setup_instance() {
    D="$BASE/instance"
    rm -rf "$D"
    mkdir -p "$D"/{config/lan-mouse,state,cache,runtime,home}
    chmod 700 "$D/runtime" "$D/home"
    cat >"$D/config/lan-mouse/config.toml" <<EOF
port = $PORT
discovery_port = $DISCOVERY_PORT
capture_backend = "dummy"
emulation_backend = "dummy"
EOF
}

inst_run() {
    env \
        HOME="$D/home" \
        XDG_CONFIG_HOME="$D/config" \
        XDG_STATE_HOME="$D/state" \
        XDG_CACHE_HOME="$D/cache" \
        XDG_RUNTIME_DIR="$D/runtime" \
        LAN_MOUSE_LOG_LEVEL="${LAN_MOUSE_LOG_LEVEL:-debug}" \
        "$@"
}

marker() { printf '%s/runtime/lan-mouse/%s' "$D" "$1"; }

write_cursor_markers() {
    mkdir -p "$D/runtime/lan-mouse"
    printf '1\n' >"$(marker cursor-hide)"
    printf '1\n' >"$(marker cursor-park)"
}

assert_no_cursor_markers() {
    local label="$1"
    check_absent "$label: cursor-hide removido" "$(marker cursor-hide)"
    check_absent "$label: cursor-park removido" "$(marker cursor-park)"
}

# Wait until the freshly spawned daemon answers over IPC. A killed instance can
# leave a stale Unix socket behind, so socket existence alone is not readiness;
# `cli list` proves a live listener accepted the request.
#
# The daemon is spawned as a direct `env ... &` command: backgrounding a shell
# function would make `$!` the wrapper subshell, and the SIGTERM cleanup below
# would then never reach the actual daemon process.
start_daemon() {
    : >"$D/daemon.log"
    env \
        HOME="$D/home" \
        XDG_CONFIG_HOME="$D/config" \
        XDG_STATE_HOME="$D/state" \
        XDG_CACHE_HOME="$D/cache" \
        XDG_RUNTIME_DIR="$D/runtime" \
        LAN_MOUSE_LOG_LEVEL="${LAN_MOUSE_LOG_LEVEL:-debug}" \
        "$BIN" daemon >>"$D/daemon.log" 2>&1 &
    DAEMON_PID=$!
    local i
    for i in $(seq 1 200); do
        if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
            wait "$DAEMON_PID" 2>/dev/null
            DAEMON_PID=""
            fail "daemon morreu antes de abrir o socket IPC"
            tail -20 "$D/daemon.log" | sed 's/^/    /'
            return 1
        fi
        if inst_run "$BIN" cli list >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.1
    done
    fail "daemon não respondeu ao IPC em 20 s"
    tail -20 "$D/daemon.log" | sed 's/^/    /'
    return 1
}

# Send `signal`, wait up to `grace` seconds for the exit and return the wait
# status (0 = clean exit). Reports a failure if the process had to be killed.
stop_daemon() {
    local signal="$1" grace="$2" i status
    [ -n "$DAEMON_PID" ] || return 0
    kill "-$signal" "$DAEMON_PID" 2>/dev/null || true
    for i in $(seq 1 $((grace * 10))); do
        if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
            break
        fi
        sleep 0.1
    done
    if kill -0 "$DAEMON_PID" 2>/dev/null; then
        fail "daemon não encerrou ${grace}s depois de SIG$signal"
        kill -KILL "$DAEMON_PID" 2>/dev/null || true
    fi
    wait "$DAEMON_PID" 2>/dev/null
    status=$?
    DAEMON_PID=""
    return "$status"
}

# Wait for an externally requested shutdown (e.g. `cli shutdown`) to complete.
wait_for_exit() {
    local grace="$1" i status
    [ -n "$DAEMON_PID" ] || return 0
    for i in $(seq 1 $((grace * 10))); do
        if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
            break
        fi
        sleep 0.1
    done
    if kill -0 "$DAEMON_PID" 2>/dev/null; then
        fail "daemon não encerrou ${grace}s depois do pedido de shutdown"
        kill -KILL "$DAEMON_PID" 2>/dev/null || true
    fi
    wait "$DAEMON_PID" 2>/dev/null
    status=$?
    DAEMON_PID=""
    return "$status"
}

check_clean_exit() {
    local label="$1" status="$2"
    if [ "$status" -eq 0 ]; then
        pass "$label: daemon saiu com código 0"
    else
        fail "$label: daemon saiu com código $status (esperado 0)"
    fi
}

check_log_contains() {
    local name="$1" pattern="$2"
    if grep -q -- "$pattern" "$D/daemon.log" 2>/dev/null; then
        pass "$name"
    else
        fail "$name (log do daemon não contém '$pattern')"
    fi
}

cleanup() {
    if [ -n "$DAEMON_PID" ]; then
        kill -KILL "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
        DAEMON_PID=""
    fi
    if [ "$KEEP" = "1" ]; then
        note "mantido para inspeção: $BASE"
    else
        rm -rf "$BASE"
    fi
}
trap cleanup EXIT

# --- scenarios -------------------------------------------------------------

# A daemon that was SIGKILLed/crashed cannot run its destructor. The helpers
# remove both markers on the next start; otherwise the extension starts with a
# permanently hidden/parked cursor.
scenario_stale_start() {
    say ""
    say "### stale_start: marcadores de um daemon morto não sobrevivem ao próximo start"
    setup_instance
    mkdir -p "$D/runtime/lan-mouse"
    write_cursor_markers
    check_present "pré-condição: marcador velho existe antes do start" "$(marker cursor-hide)"

    start_daemon || return 1
    assert_no_cursor_markers "start"
    stop_daemon TERM 5
    check_clean_exit "SIGTERM" "$?"
    check_log_contains "SIGTERM: shutdown gracioso registado" "terminating service"
    assert_no_cursor_markers "SIGTERM"
}

# The reported bug: closing the app while a capture/return was active left the
# extension hiding the cursor. A clean shutdown must remove both markers.
scenario_sigterm_release() {
    say ""
    say "### sigterm_release: SIGTERM limpa heartbeat e park (cursor volta)"
    setup_instance
    start_daemon || return 1
    write_cursor_markers
    check_present "pré-condição: heartbeat ativo" "$(marker cursor-hide)"
    check_present "pré-condição: cursor parked" "$(marker cursor-park)"

    stop_daemon TERM 5
    local status=$?
    check_clean_exit "SIGTERM" "$status"
    check_log_contains "SIGTERM: shutdown gracioso registado" "terminating service"
    assert_no_cursor_markers "SIGTERM"
}

# Explicit quit (`lan-mouse cli shutdown`, used by the GUI and the installer
# upgrade) takes the same shutdown path over IPC.
scenario_cli_shutdown_release() {
    say ""
    say "### cli_shutdown_release: 'cli shutdown' limpa heartbeat e park"
    setup_instance
    start_daemon || return 1
    write_cursor_markers

    if inst_run "$BIN" cli shutdown >"$D/shutdown.log" 2>&1; then
        pass "cli shutdown respondeu com sucesso"
    else
        fail "cli shutdown falhou: $(tail -3 "$D/shutdown.log" | tr '\n' ' ')"
    fi
    wait_for_exit 10
    check_clean_exit "cli shutdown" "$?"
    check_log_contains "cli shutdown: shutdown gracioso registado" "terminating service"
    assert_no_cursor_markers "cli shutdown"
}

# SIGKILL documents the crash window (no destructor runs) and proves the next
# start repairs it. The heartbeat itself is ignored by the extension after 3 s,
# but a stale park marker would hide the cursor until the pointer moves.
scenario_crash_restart() {
    say ""
    say "### crash_restart: SIGKILL deixa marcadores; o próximo start limpa"
    setup_instance
    start_daemon || return 1
    write_cursor_markers

    stop_daemon KILL 5
    local status=$?
    if [ "$status" -eq 137 ] || [ "$status" -eq 9 ]; then
        pass "SIGKILL: processo terminou por sinal (status $status)"
    else
        fail "SIGKILL: status inesperado $status (esperado 137/9)"
    fi
    check_present "marcador sobrevive ao crash (janela documentada)" "$(marker cursor-hide)"
    check_present "park sobrevive ao crash (janela documentada)" "$(marker cursor-park)"

    start_daemon || return 1
    assert_no_cursor_markers "restart"
    stop_daemon TERM 5 >/dev/null
    assert_no_cursor_markers "restart+SIGTERM"
}

# No capture/incoming session => the service must not publish a hide
# heartbeat. This is the idle cursor-latch regression (R-24) at file level.
scenario_idle_no_hide() {
    say ""
    say "### idle_no_hide: daemon ocioso não publica heartbeat (cursor visível)"
    setup_instance
    start_daemon || return 1
    check_absent "sem captura: sem cursor-hide" "$(marker cursor-hide)"
    check_absent "sem sessão: sem cursor-park" "$(marker cursor-park)"
    # More than one heartbeat interval: a periodic refresh with no capture
    # would show up here.
    sleep 1.2
    check_absent "após 1.2 s ocioso: sem cursor-hide" "$(marker cursor-hide)"
    stop_daemon TERM 5 >/dev/null
}

# --- main ------------------------------------------------------------------

ALL_SCENARIOS=(stale_start sigterm_release cli_shutdown_release crash_restart idle_no_hide)

say "==> lan-mouse QA: hand-off / shutdown cursor lifecycle"
say "    repo: $REPO_ROOT"
say "    base: $BASE"
say "    porta: $PORT"

if [ "$SKIP_BUILD" != "1" ]; then
    say ""
    say "==> build headless (no-default-features)"
    if ! cargo build -p lan-mouse --no-default-features; then
        say "ERRO: build headless falhou; nada foi testado." >&2
        exit 1
    fi
fi
if [ ! -x "$BIN" ]; then
    say "ERRO: binário não encontrado em $BIN (rode sem QA_SKIP_BUILD=1)." >&2
    exit 1
fi

if [ "$#" -gt 0 ]; then
    SCENARIOS=("$@")
else
    SCENARIOS=("${ALL_SCENARIOS[@]}")
fi

mkdir -p "$BASE"
for scenario in "${SCENARIOS[@]}"; do
    if ! declare -F "scenario_$scenario" >/dev/null; then
        fail "cenário desconhecido: $scenario"
        continue
    fi
    "scenario_$scenario"
    if [ -n "$DAEMON_PID" ]; then
        stop_daemon TERM 5 >/dev/null
    fi
done

say ""
say "=========================================================="
if [ "$FAILS" -eq 0 ]; then
    say "RESULTADO: PASS ($CHECKS checks)"
else
    say "RESULTADO: FAIL ($FAILS de $CHECKS checks)"
fi
say "=========================================================="

if [ "$FAILS" -gt 0 ]; then
    exit "$FAILS"
fi
exit 0
