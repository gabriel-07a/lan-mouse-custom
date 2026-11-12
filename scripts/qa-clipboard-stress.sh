#!/usr/bin/env bash
# Repeat the independent clipboard atomicity harness to expose flakes and
# timing/ordering assumptions.
#
# The harness (tests/qa_clipboard_atomicity.rs) is deterministic by design:
# simulated clock, seeded/periodic loss, no sleeps, no sockets, no threads.
# Repeating it still catches accidental nondeterminism (HashMap iteration,
# parallel-test interference, assumptions added later), which is why this is a
# separate stress entry point instead of one `cargo test` call.
#
# Usage:
#   scripts/qa-clipboard-stress.sh                    # 20 runs, 4 threads
#   scripts/qa-clipboard-stress.sh --runs 50          # longer flake hunt
#   scripts/qa-clipboard-stress.sh --threads 1        # fully serialized
#   scripts/qa-clipboard-stress.sh --with-unit        # also repeat the
#                                                     # input-clipboard and
#                                                     # proto clipboard unit tests
#   scripts/qa-clipboard-stress.sh --include-ignored  # residual/robustness
#                                                     # checks (may be red
#                                                     # until the fix covers
#                                                     # them; see the OS matrix)
#   scripts/qa-clipboard-stress.sh --skip <name> ...  # libtest --skip filter
#
# The two acceptance tests for the reported symptom ("first copy after a
# reconnect is lost, the second Ctrl+C works") are expected to be red until the
# atomicity fix lands. Skip them explicitly while validating unrelated changes:
#   scripts/qa-clipboard-stress.sh \
#     --skip reconnect_after_an_interrupted_transfer_resyncs_the_current_clipboard \
#     --skip copying_again_after_reconnect_is_offered_again
#
# Exit status: 0 only if every run passed. The failing run number and the exact
# failed tests are printed; there is no retry that hides a flake.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Repeated debug/release cycles must not fill the disk with incremental state.
export CARGO_INCREMENTAL=0

runs=20
threads=4
with_unit=0
include_ignored=0
skip_args=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --runs) runs="${2:?--runs needs a value}"; shift 2 ;;
        --threads) threads="${2:?--threads needs a value}"; shift 2 ;;
        --with-unit) with_unit=1; shift ;;
        --include-ignored) include_ignored=1; shift ;;
        --skip) skip_args+=(--skip "${2:?--skip needs a test name}"); shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [[ ! "$runs" =~ ^[1-9][0-9]*$ ]]; then
    echo "--runs must be a positive integer" >&2
    exit 2
fi
if [[ ! "$threads" =~ ^[1-9][0-9]*$ ]]; then
    echo "--threads must be a positive integer" >&2
    exit 2
fi

# Determinism guard: the harness must not synchronise with sleeps or threads.
if grep -nE 'std::thread|thread::sleep|tokio::time::sleep|std::time::Sleep' \
    tests/qa_clipboard_atomicity.rs; then
    echo "determinism violation in tests/qa_clipboard_atomicity.rs (sleep/thread found)" >&2
    exit 1
fi

# Build once so the timed runs only execute the already-built test binary.
cargo test --test qa_clipboard_atomicity --no-run

failed_runs=0
for run in $(seq 1 "$runs"); do
    echo "=== clipboard atomicity run $run/$runs (threads=$threads) ==="
    if ! cargo test --test qa_clipboard_atomicity -- \
        --test-threads="$threads" ${skip_args[@]+"${skip_args[@]}"}; then
        failed_runs=$((failed_runs + 1))
    fi
done

if [[ "$with_unit" -eq 1 ]]; then
    for run in $(seq 1 "$runs"); do
        echo "=== clipboard sync/primitives unit run $run/$runs ==="
        if ! cargo test -p input-clipboard --lib -- --test-threads="$threads"; then
            failed_runs=$((failed_runs + 1))
        fi
        if ! cargo test -p lan-mouse-proto clipboard --lib -- --test-threads="$threads"; then
            failed_runs=$((failed_runs + 1))
        fi
    done
fi

if [[ "$include_ignored" -eq 1 ]]; then
    echo "=== residual/robustness tests (--ignored) ==="
    if ! cargo test --test qa_clipboard_atomicity -- --ignored --test-threads=1; then
        failed_runs=$((failed_runs + 1))
    fi
fi

if [[ "$failed_runs" -gt 0 ]]; then
    echo "$failed_runs clipboard atomicity run(s) failed" >&2
    exit 1
fi
echo "all $runs clipboard atomicity run(s) passed"
