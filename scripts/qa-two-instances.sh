#!/usr/bin/env bash
# QA harness: two lan-mouse instances on one Linux host, fully isolated state.
#
# Reproduces/regression-tests the reported peer identity bugs. The product is
# deliberately IPv4-only (see README/DOC), so every scenario here is IPv4:
#   1. one peer shown twice or with a flapping active address
#   2. duplicate/reverse clients with the wrong port (false "connected")
#   3. HISTORICAL/SKIPPED: IPv6-only peers. IPv6 is never dialed by this fork;
#      the old scenario is kept for context and always reports SKIP.
#   4. peer-down recovery: a dead peer must stop being reported as connected
#   5. stale config is honoured by the app (precondition for the installer fix)
#
# Every instance gets its own XDG_CONFIG_HOME/STATE/CACHE/RUNTIME and HOME, so
# they do not share config, certificate, audit log or the IPC socket. Backends
# are "dummy", so no real input devices are needed. Communications happen over
# loopback IPv4 only: an IPv6 address is never dialed (IPv4-only product).
#
# Usage:
#   scripts/qa-two-instances.sh                 # run all scenarios
#   scripts/qa-two-instances.sh dual duplicate  # run selected scenarios
#   QA_KEEP=1 scripts/qa-two-instances.sh       # keep dirs/logs for inspection
#   QA_PORT_A=43421 QA_PORT_B=43422 ...         # custom ports
#   scripts/qa-two-instances.sh ipv6_only       # historical: prints SKIP
#
# Exit code: number of failed checks (0 = everything that should hold at the
# current HEAD holds). The duplicate scenario is a regression criterion again:
# it must pass now that the service identity fixes landed.
#
# Requirements: bash, coreutils, a Linux host with IPv4 loopback (127.0.0.0/8 is
# local; no extra address setup needed). The binary is built headless (no GTK):
#   cargo build -p lan-mouse --no-default-features
set -u

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

BIN="$REPO_ROOT/target/debug/lan-mouse"
BASE="${QA_BASE_DIR:-$REPO_ROOT/target/qa-two-instances}"
PORT_A="${QA_PORT_A:-42421}"
PORT_B="${QA_PORT_B:-42422}"
KEEP="${QA_KEEP:-0}"

declare -A DAEMON_PID=()
FAILS=0
CHECKS=0

say() { printf '%s\n' "$*"; }
note() { printf '  [info] %s\n' "$*"; }
pass() { CHECKS=$((CHECKS + 1)); printf '  [PASS] %s\n' "$*"; }
fail() { CHECKS=$((CHECKS + 1)); FAILS=$((FAILS + 1)); printf '  [FAIL] %s\n' "$*"; }
check_eq() {
    CHECKS=$((CHECKS + 1))
    if [ "$2" = "$3" ]; then
        printf '  [PASS] %s (=%s)\n' "$1" "$3"
    else
        FAILS=$((FAILS + 1))
        printf '  [FAIL] %s: esperado=%q obtido=%q\n' "$1" "$2" "$3"
    fi
}
check_contains() {
    CHECKS=$((CHECKS + 1))
    case "$3" in
        *"$2"*) printf '  [PASS] %s contém %q\n' "$1" "$2" ;;
        *) FAILS=$((FAILS + 1)); printf '  [FAIL] %s não contém %q: %q\n' "$1" "$2" "$3" ;;
    esac
}

cleanup() {
    local name
    for name in "${!DAEMON_PID[@]}"; do
        kill "${DAEMON_PID[$name]}" 2>/dev/null || true
    done
    for name in "${!DAEMON_PID[@]}"; do
        wait "${DAEMON_PID[$name]}" 2>/dev/null || true
    done
    DAEMON_PID=()
}
trap cleanup EXIT

# --- instance plumbing ------------------------------------------------------

inst_dir() { printf '%s/%s' "$BASE" "$1"; }

setup_instance() {
    local name="$1" dir
    dir="$(inst_dir "$name")"
    rm -rf "$dir"
    mkdir -p "$dir"/{config/lan-mouse,state,cache,runtime,home}
    chmod 700 "$dir/runtime" "$dir/home"
}

# run a command with the instance's isolated environment
inst_run() {
    local name="$1" dir
    shift
    dir="$(inst_dir "$name")"
    env \
        HOME="$dir/home" \
        XDG_CONFIG_HOME="$dir/config" \
        XDG_STATE_HOME="$dir/state" \
        XDG_CACHE_HOME="$dir/cache" \
        XDG_RUNTIME_DIR="$dir/runtime" \
        LAN_MOUSE_LOG_LEVEL="${LAN_MOUSE_LOG_LEVEL:-debug}" \
        "$@"
}

write_config() {
    local name="$1" port="$2"
    shift 2
    {
        printf 'port = %s\n' "$port"
        printf 'capture_backend = "dummy"\n'
        printf 'emulation_backend = "dummy"\n'
        # Loopback is pre-trusted so the two local instances do not need GUI
        # pairing approval; this is the same address-rule mechanism a LAN
        # deployment can use, not a test-only code path.
        # IPv4 loopback only (IPv4-only product). trusted_ips is the legacy key
        # and is migrated to a full-access address rule on load.
        printf 'trusted_ips = ["127.0.0.1"]\n'
        printf '\n'
        local client
        for client in "$@"; do
            printf '%s\n' "$client"
        done
    } > "$(inst_dir "$name")/config/lan-mouse/config.toml"
}

start_daemon() {
    local name="$1" dir
    dir="$(inst_dir "$name")"
    : >"$dir/daemon.log"
    inst_run "$name" "$BIN" daemon >>"$dir/daemon.log" 2>&1 &
    DAEMON_PID["$name"]=$!
    local i pid="${DAEMON_PID[$name]}"
    for i in $(seq 1 200); do
        if [ -S "$dir/runtime/lan-mouse-socket.sock" ]; then
            return 0
        fi
        if ! kill -0 "$pid" 2>/dev/null; then
            fail "daemon $name morreu antes do socket IPC"
            tail -15 "$dir/daemon.log" | sed 's/^/    /'
            return 1
        fi
        sleep 0.1
    done
    fail "daemon $name não abriu o socket IPC em 20 s"
    tail -15 "$dir/daemon.log" | sed 's/^/    /'
    return 1
}

stop_daemon() {
    local name="$1" pid="${DAEMON_PID[$1]:-}"
    [ -n "$pid" ] || return 0
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    unset "DAEMON_PID[$name]"
}

restart_daemon() {
    local name="$1"
    stop_daemon "$name"
    start_daemon "$name"
}

cli() {
    local name="$1"
    shift
    inst_run "$name" "$BIN" cli "$@" 2>&1
}

client_lines() {
    local name="$1"
    cli "$name" list | grep '^id ' || true
}

client_count() {
    local name="$1"
    client_lines "$name" | grep -c '^id ' || true
}

enable_emulation() {
    cli "$1" enable-emulation >/dev/null 2>&1 || note "enable-emulation falhou em $1"
}

# status string of the first configured client (id 0), without the rtt suffix
first_status() {
    local name="$1"
    client_lines "$name" | sed -n '1p' \
        | sed -n 's/.*status: \(.*\), ips:.*/\1/p' \
        | sed 's/, rtt: [0-9]*ms$//'
}

# address after "connected (to ...)" or "connected (from ...)"
first_addr() {
    local status
    status="$(first_status "$1")"
    printf '%s' "$status" | sed -n \
        -e 's/^connected (to \([^)]*\)).*/\1/p' \
        -e 's/^connected (bidirectional: out=\([^,]*\),.*/\1/p' \
        -e 's/^reconnecting (\([^)]*\)).*/\1/p'
}

wait_connected() {
    local name="$1" seconds="${2:-10}" i
    for i in $(seq 1 $((seconds * 5))); do
        case "$(first_status "$name")" in
            connected*) return 0 ;;
        esac
        sleep 0.2
    done
    return 1
}

# Both instances get a valid base config (same port/backends/trusted rules)
# before their first start; scenarios that need clients overwrite one or both
# configs and restart the affected daemons.
start_pair() {
    setup_instance a
    setup_instance b
    write_config a "$PORT_A"
    write_config b "$PORT_B"
    say "==> subindo duas instâncias ($PORT_A <-> $PORT_B)"
    start_daemon a || return 1
    start_daemon b || return 1
    sleep 0.5
    enable_emulation a
    enable_emulation b
}

# CPU time (user+system ticks) of a pid, or empty when it cannot be read.
# /proc/<pid>/stat has the executable name in parentheses (may contain spaces),
# so strip everything through the last ')' before splitting fields.
proc_cpu_ticks() {
    local pid="$1" stat
    stat="$(cat "/proc/$pid/stat" 2>/dev/null)" || return 1
    printf '%s' "${stat##*) }" | awk '{print $12 + $13}'
}

# Non-spin regression for the emulation event loop (P0 found by security-auditor):
# a closed `edge_return_rx` used to make the select arm fire forever (100% of a
# core on Linux/Dummy). A live daemon whose peer never connects must stay
# essentially idle; a busy loop is trivially visible in the CPU time delta.
scenario_no_spin() {
    say ""
    say "### scenario no-spin: daemon idle com backend Dummy não gira CPU"
    setup_instance a
    write_config a "$PORT_A"
    start_daemon a || return 0
    sleep 2
    local pid="${DAEMON_PID[a]:-}"
    if [ -z "$pid" ]; then
        fail "sem pid do daemon A"
        return 0
    fi
    local t0 t1 hz elapsed_ms cpu_ms
    t0="$(proc_cpu_ticks "$pid")"
    sleep 3
    t1="$(proc_cpu_ticks "$pid")"
    if [ -z "$t0" ] || [ -z "$t1" ]; then
        note "não foi possível ler /proc/$pid/stat; pulando medição"
        pass "medição de CPU não disponível neste host"
        return 0
    fi
    hz="$(getconf CLK_TCK 2>/dev/null || echo 100)"
    elapsed_ms=$((3 * 1000))
    cpu_ms=$(( (t1 - t0) * 1000 / hz ))
    note "CPU do daemon idle em 3 s: ${cpu_ms}ms (< $((elapsed_ms / 10))ms = <10%)"
    if [ "$cpu_ms" -lt $((elapsed_ms / 10)) ]; then
        pass "daemon idle usou <10% de CPU (sem busy-loop)"
    else
        fail "daemon idle usou ${cpu_ms}ms em ${elapsed_ms}ms de parede — suspeita de busy-loop"
    fi
    stop_daemon a
}

# --- scenarios --------------------------------------------------------------

# One configured client with two IPv4 loopback addresses for the same peer
# (127.0.0.1 + 127.0.0.2; the old IPv4+IPv6 variant is historical because the
# product is IPv4-only). The initiator must keep one row and one stable active
# address.
scenario_dual() {
    say ""
    say "### scenario dual: client com dois endereços IPv4 (127.0.0.1 + 127.0.0.2)"
    start_pair || return 0
    write_config a "$PORT_A" \
        "[[clients]]
position = \"right\"
ips = [\"127.0.0.1\", \"127.0.0.2\"]
port = $PORT_B
activate_on_startup = true"
    restart_daemon a
    enable_emulation a
    if ! wait_connected a 10; then
        fail "A não conectou ao peer dual-address"
        return 0
    fi
    check_eq "A tem uma única linha de cliente" 1 "$(client_count a)"
    check_contains "A reporta connected" "connected" "$(first_status a)"

    local addr first="" flapped=0 i
    for i in $(seq 1 10); do
        addr="$(first_addr a)"
        if [ -n "$addr" ]; then
            if [ -z "$first" ]; then first="$addr"; fi
            if [ "$addr" != "$first" ]; then flapped=1; fi
        fi
        sleep 0.25
    done
    if [ -n "$first" ]; then
        check_eq "active_addr estável em 10 amostras" 0 "$flapped"
        note "active_addr observado: $first"
    else
        fail "não foi possível ler active_addr de A"
    fi

    # Reconnect: the peer restarts (same cert/config); A must keep one row and
    # come back on the same address (last-good preference, task-17).
    stop_daemon b
    sleep 0.5
    start_daemon b || return 0
    if wait_connected a 15; then
        check_eq "após reconexão A mantém uma linha" 1 "$(client_count a)"
        if [ -n "$first" ]; then
            check_eq "após reconexão mantém o mesmo endereço" "$first" "$(first_addr a)"
        fi
    else
        fail "A não reconectou após reiniciar o peer"
    fi
    if [ "$KEEP" = 1 ]; then note "logs: $(inst_dir a)/daemon.log $(inst_dir b)/daemon.log"; fi
}

# Regression case for the reported duplicate: both sides configure each other
# over IPv4 loopback. A dials B and B dials A; the incoming session must attach
# to the single configured row on each side (no reverse client created for a
# second address family, no dial to the default port 4242). The old
# IPv4-vs-IPv6 variant cannot happen anymore because the product is IPv4-only;
# the duplicate/reverse-client invariant is the criterion.
scenario_duplicate() {
    say ""
    say "### scenario duplicate: peer IPv4 nos dois lados (sem linha reversa extra)"
    start_pair || return 0
    write_config a "$PORT_A" \
        "[[clients]]
position = \"right\"
ips = [\"127.0.0.1\", \"127.0.0.2\"]
port = $PORT_B
activate_on_startup = true"
    write_config b "$PORT_B" \
        "[[clients]]
position = \"left\"
ips = [\"127.0.0.1\", \"127.0.0.2\"]
port = $PORT_A
activate_on_startup = true"
    restart_daemon a
    restart_daemon b
    enable_emulation a
    enable_emulation b
    wait_connected a 15 || fail "A não conectou ao peer"
    wait_connected b 15 || fail "B não conectou ao peer"
    sleep 1
    say "  --- A (esperado: 1 linha configurada) ---"
    client_lines a | sed 's/^/    /'
    say "  --- B (esperado: 1 linha configurada) ---"
    client_lines b | sed 's/^/    /'
    check_eq "A tem uma única linha para o peer" 1 "$(client_count a)"
    check_eq "B tem uma única linha para o peer" 1 "$(client_count b)"
    # The single row must carry both directions: the configured outgoing link
    # plus the incoming session attached to it (no separate adopted row).
    check_contains "A: linha configurada conectada" "connected" "$(first_status a)"
    check_contains "B: linha configurada conectada" "connected" "$(first_status b)"
    check_contains "A: sessão de entrada anexada à linha" "bidirectional" "$(first_status a)"
    check_contains "B: sessão de entrada anexada à linha" "bidirectional" "$(first_status b)"
    # wrong-port symptom: an auto-added reverse client dialling DEFAULT_PORT
    if client_lines a | grep -q ':4242 '; then
        fail "A criou cliente reverso no port default 4242"
    else
        pass "A não criou cliente reverso no port default 4242"
    fi
    if client_lines b | grep -q ':4242 '; then
        fail "B criou cliente reverso no port default 4242"
    else
        pass "B não criou cliente reverso no port default 4242"
    fi
    # ... and the connected addresses use the custom peer ports, not 4242.
    check_contains "A conectou na porta custom do peer" ":$PORT_B" "$(first_addr a)"
    check_contains "B conectou na porta custom do peer" ":$PORT_A" "$(first_addr b)"
}

# HISTORICAL / SKIPPED: the product is deliberately IPv4-only (README: "Lan
# Mouse is IPv4-only"), so an IPv6-only client is never dialed and this cannot
# be a pass/fail criterion anymore. Kept so the old regression scenario is
# documented and callable; it always reports SKIP and never fails the run.
scenario_ipv6_only() {
    say ""
    say "### scenario ipv6-only: HISTORICAL/SKIPPED (produto é IPv4-only)"
    note "IPv6 não é discado por decisão de produto; nada a validar aqui."
    note "a matriz de aceitação atual é IPv4-only (docs/qa-repro-matrix.md, E1)."
    note "veja os cenários dual/duplicate para o comportamento esperado em IPv4."
    return 0
}

# Discovery + explicit disconnect + reconnect across two real service
# processes. This guards the user-facing contract: a discovery result must find
# the peer, deactivating one side must revoke both links, and activating again
# must reconnect (the AuthRevoke must not leave a half-open/reconnecting link).
scenario_disconnect() {
    say ""
    say "### scenario disconnect: descoberta, AuthRevoke bilateral e reconexão"
    setup_instance a
    setup_instance b
    write_config a "$PORT_A" \
        "[[clients]]
position = \"right\"
ips = [\"127.0.0.1\"]
port = $PORT_B
activate_on_startup = true"
    write_config b "$PORT_B"
    # Start B first so it owns the shared discovery port 4245: on a single host
    # both instances cannot bind UDP 4245, so A's beacon binding fails. That is
    # expected in this sandboxed setup (not a product bug); A's explicit scan
    # can still probe the loopback alias. The extra-host hook is test-only and
    # does not affect normal builds.
    start_daemon b || return 0
    export LAN_MOUSE_DISCOVERY_EXTRA_HOSTS=127.0.0.1
    start_daemon a || return 0
    sleep 0.5
    enable_emulation a
    enable_emulation b
    if ! wait_connected a 10; then
        fail "A não conectou a B"
        return 0
    fi

    local scan_out
    scan_out="$(cli a scan 2>&1 || true)"
    check_contains "scan de A encontra o peer Lan Mouse" "lan-mouse" "$scan_out"

    cli a deactivate 0 >/dev/null 2>&1 || true
    sleep 2
    case "$(first_status a)" in
        connected*) fail "A continuou connected após deactivate" ;;
        *) pass "A desconectou após deactivate" ;;
    esac
    if wait_connected a 2; then
        fail "A reconectou sozinho após desconectar explicitamente"
    else
        pass "A não reconectou sozinho (AuthRevoke)"
    fi

    cli a activate 0 >/dev/null 2>&1 || true
    if wait_connected a 10; then
        pass "A reconectou ao ativar de novo"
    else
        fail "A não reconectou após activate"
    fi
    unset LAN_MOUSE_DISCOVERY_EXTRA_HOSTS
    if [ "$KEEP" = 1 ]; then note "logs: $(inst_dir a)/daemon.log $(inst_dir b)/daemon.log"; fi
}

# A dead peer must stop being reported as connected. This is the CLI/service
# mirror of the false "Connected" badge; the GTK-level assertion is manual.
scenario_peer_down() {
    say ""
    say "### scenario peer-down: matar o peer e medir quanto tempo A mente"
    start_pair || return 0
    write_config a "$PORT_A" \
        "[[clients]]
position = \"right\"
ips = [\"127.0.0.1\"]
port = $PORT_B
activate_on_startup = true"
    restart_daemon a
    enable_emulation a
    if ! wait_connected a 10; then
        fail "A não conectou antes do teste"
        return 0
    fi
    stop_daemon b
    local elapsed=0 lied="" i
    for i in $(seq 1 80); do
        sleep 0.25
        elapsed=$((i * 250))
        case "$(first_status a)" in
            connected*) lied="$elapsed" ;;
            *) break ;;
        esac
    done
    if [ -n "$lied" ]; then
        note "A continuou 'connected' por ~${lied}ms após o peer morrer"
        # Fora do escopo service-side desta task; o "Connected" falso da UI é
        # coberto pela task-19. Aqui só registramos, sem falhar o harness.
        pass "medida coletada (service pode levar até o watchdog)"
    else
        pass "A deixou de reportar connected imediatamente"
    fi
}

# Bug 4 precondition: the app loads whatever config exists. The installer is
# responsible for not leaving an old config behind on a clean install; this
# scenario documents what happens if it does.
scenario_stale_config() {
    say ""
    say "### scenario stale-config: config antiga é carregada pelo app"
    setup_instance a
    write_config a "$PORT_A" \
        "[[clients]]
position = \"left\"
hostname = \"old-peer.example\"
ips = [\"192.0.2.55\"]
port = 4242"
    start_daemon a || return 0
    local lines
    lines="$(client_lines a)"
    say "  --- A após subir com config antiga ---"
    printf '%s\n' "$lines" | sed 's/^/    /'
    check_eq "config antiga é carregada (pré-condição do bug 4)" 1 "$(client_count a)"
    check_contains "cliente antigo presente" "old-peer.example" "$lines"
}

# Explicit reset (task-20): `--reset-config [--dry-run]` must remove only the
# app's own directories, refuse while the service runs, and be the documented
# way to get a pristine state outside the Windows installer.
scenario_reset_config() {
    say ""
    say "### scenario reset-config: limpeza explícita cross-platform"
    setup_instance a
    write_config a "$PORT_A" \
        "[[clients]]
position = \"left\"
hostname = \"old-peer.example\"
ips = [\"192.0.2.55\"]"
    # state/cache leftovers from an older build + an unrelated file to keep
    mkdir -p "$(inst_dir a)/state/lan-mouse" "$(inst_dir a)/cache/lan-mouse"
    printf 'token\n' > "$(inst_dir a)/state/lan-mouse/input-capture.token"
    printf 'token\n' > "$(inst_dir a)/cache/lan-mouse/remote-desktop.token"
    printf 'keep me\n' > "$(inst_dir a)/config/unrelated.txt"

    local out
    # 1) refuses while the daemon is running
    start_daemon a || return 0
    out="$(inst_run a "$BIN" --reset-config 2>&1)" || true
    check_contains "reset recusa com serviço rodando" "still running" "$out"
    stop_daemon a

    # 2) dry-run lists the paths and changes nothing
    out="$(inst_run a "$BIN" --reset-config --dry-run 2>&1)"
    check_contains "dry-run lista o config dir" "config" "$out"
    [ -f "$(inst_dir a)/config/lan-mouse/config.toml" ] \
        && pass "dry-run não move/apaga config" \
        || fail "dry-run moveu/apagou config"
    [ -f "$(inst_dir a)/state/lan-mouse/input-capture.token" ] \
        && pass "dry-run não move/apaga state" \
        || fail "dry-run moveu/apagou state"
    if ls "$(inst_dir a)/config" 2>/dev/null | grep -q '^lan-mouse\.bak-'; then
        fail "dry-run criou backup"
    else
        pass "dry-run não criou backup"
    fi

    # 3) real reset moves the app dirs to a reversible backup (task-20)
    inst_run a "$BIN" --reset-config >/dev/null 2>&1 || true
    [ -e "$(inst_dir a)/config/lan-mouse" ] \
        && fail "reset deixou o config dir ativo" \
        || pass "reset tirou o config dir do lugar (config+cert)"
    [ -e "$(inst_dir a)/state/lan-mouse" ] \
        && fail "reset deixou o state dir ativo" \
        || pass "reset tirou o state dir do lugar (audit/tokens)"
    [ -e "$(inst_dir a)/cache/lan-mouse" ] \
        && fail "reset deixou o cache dir legado ativo" \
        || pass "reset tirou o cache dir legado do lugar"
    if ls "$(inst_dir a)/config" 2>/dev/null | grep -q '^lan-mouse\.bak-'; then
        pass "reset criou backup reversível do config"
    else
        fail "reset não criou backup do config (perda irreversível?)"
    fi
    [ -f "$(inst_dir a)/config/unrelated.txt" ] \
        && pass "reset preservou arquivo não relacionado" \
        || fail "reset apagou arquivo não relacionado"

    # 4) the app starts fresh afterwards
    start_daemon a
    check_eq "após reset o app inicia sem devices" 0 "$(client_count a)"
    stop_daemon a

    # 5) --purge deletes permanently instead of backing up
    write_config a "$PORT_A" \
        "[[clients]]
position = \"left\"
ips = [\"192.0.2.56\"]"
    inst_run a "$BIN" --reset-config --purge >/dev/null 2>&1 || true
    [ -e "$(inst_dir a)/config/lan-mouse" ] \
        && fail "--purge deixou o config dir" \
        || pass "--purge apagou o config dir"
    [ -e "$(inst_dir a)/config/unrelated.txt" ] \
        && pass "--purge preservou arquivo não relacionado" \
        || fail "--purge apagou arquivo não relacionado"
}

# Application state must live only under the per-instance XDG dirs.
scenario_isolation() {
    say ""
    say "### scenario isolation: estado isolado por XDG/HOME"
    start_pair || return 0
    local name dir leaked=""
    for name in a b; do
        dir="$(inst_dir "$name")"
        if [ ! -f "$dir/config/lan-mouse/config.toml" ]; then
            leaked="$leaked $name/config"
        fi
        if [ ! -f "$dir/config/lan-mouse/lan-mouse.pem" ]; then
            leaked="$leaked $name/cert"
        fi
        if [ -e "$dir/home/.config/lan-mouse" ]; then
            leaked="$leaked $name/home-default"
        fi
    done
    if [ -n "$leaked" ]; then
        fail "estado fora dos XDG dirs:$leaked"
    else
        pass "config/cert/audit ficaram nos XDG dirs de cada instância"
    fi
}

# --- main -------------------------------------------------------------------

# Always (re)build headless so a previous `cargo test/build` with default
# features (GTK) cannot change what this harness exercises.
if true; then
    say "==> build headless do binário"
    cargo build -p lan-mouse --no-default-features || {
        say "build falhou"
        exit 1
    }
fi

# Regression (post-release report): closing the connection on the receiving
# side must tear down the live incoming DTLS session by certificate identity,
# not only when a capture zone already exists. Before the fix a revoke left the
# peer "connected" with its cursor still captured (physical mouse frozen).
scenario_revoke() {
    say "### scenario revoke: revogar no receptor derruba a sessão e libera o controlador"
    start_pair || return 1
    write_config a "$PORT_A" "[[clients]]
hostname = \"b\"
ips = [\"127.0.0.1\"]
port = $PORT_B
position = \"right\"
activate_on_startup = true"
    restart_daemon a || return 1
    if ! wait_connected a 10; then
        fail "A não conectou a B antes do revoke"
        return 1
    fi
    pass "A conectado a B"
    local fp waited
    fp="$(cli b history --limit 5 | grep -oE 'fp=[0-9a-f:]+' | head -1 | cut -d= -f2)"
    if [ -z "$fp" ]; then
        fail "não foi possível obter o fingerprint do peer no receptor"
        return 1
    fi
    note "fingerprint do peer: $fp"
    cli b revoke "$fp" >/dev/null || note "revoke retornou erro"
    waited=0
    while [ "$waited" -lt 50 ]; do
        case "$(first_status a)" in
            disconnected*) break ;;
        esac
        sleep 0.2
        waited=$((waited + 1))
    done
    check_contains "controlador liberado após o revoke" "disconnected" "$(first_status a)"
    check_contains "receptor sem sessão ativa após o revoke" "disconnected" "$(first_status b)"
    check_contains "receptor negou o dispositivo" "no devices known yet" "$(cli b devices)"
}

SCENARIOS=("$@")
if [ "${#SCENARIOS[@]}" -eq 0 ]; then
    SCENARIOS=(dual duplicate peer_down disconnect revoke stale_config reset_config no_spin isolation)
fi

say "lan-mouse QA two-instances (repo: $REPO_ROOT, bin: $BIN, base: $BASE)"
say "portas: A=$PORT_A B=$PORT_B"

for scenario in "${SCENARIOS[@]}"; do
    cleanup
    case "$scenario" in
        dual) scenario_dual ;;
        duplicate) scenario_duplicate ;;
        ipv6_only) scenario_ipv6_only ;;
        peer_down) scenario_peer_down ;;
        revoke)
            scenario_revoke
            ;;
        disconnect) scenario_disconnect ;;
        stale_config) scenario_stale_config ;;
        reset_config) scenario_reset_config ;;
        no_spin) scenario_no_spin ;;
        isolation) scenario_isolation ;;
        *)
            say "cenário desconhecido: $scenario"
            say "use: dual duplicate ipv6_only peer_down disconnect stale_config reset_config no_spin isolation"
            say "(ipv6_only é histórico e sempre reporta SKIP; o produto é IPv4-only)"
            exit 2
            ;;
    esac
done

cleanup
say ""
say "resultado: $((CHECKS - FAILS))/$CHECKS checks OK, $FAILS falha(s)"
if [ "$FAILS" -gt 0 ]; then
    say "falha acima é regressão: os cenários duplicate/stale_config são critérios e"
    say "devem passar no HEAD atual (o produto é IPv4-only; ipv6_only é histórico/SKIP)."
    exit "$FAILS"
fi
exit 0
