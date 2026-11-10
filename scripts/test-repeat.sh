#!/usr/bin/env bash
# Repeat the workspace test suite to expose flaky tests.
#
# Usage:
#   scripts/test-repeat.sh            # 5 runs, default features
#   scripts/test-repeat.sh --runs 20 --all-features
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Repeated release/debug cycles must not fill the disk with incremental state.
export CARGO_INCREMENTAL=0

runs=5
cargo_args=(--workspace)
while [[ $# -gt 0 ]]; do
    case "$1" in
        --runs) runs="${2:?--runs needs a value}"; shift 2 ;;
        --all-features) cargo_args+=(--all-features); shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

for run in $(seq 1 "$runs"); do
    echo "=== test run $run/$runs ==="
    cargo test "${cargo_args[@]}"
done
echo "all $runs runs passed"
