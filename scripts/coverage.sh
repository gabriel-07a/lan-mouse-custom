#!/usr/bin/env bash
# Run the workspace test suite under cargo-llvm-cov and write an LCOV report.
#
# Requires: cargo-llvm-cov (`cargo install cargo-llvm-cov` or
# taiki-e/install-action@cargo-llvm-cov).
#
# Usage:
#   scripts/coverage.sh                 # report only
#   scripts/coverage.sh --check 75      # fail below 75% line coverage
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

OUT_DIR="${COVERAGE_DIR:-target/coverage}"
mkdir -p "$OUT_DIR"

# The generated fluidity matrix is ~2000 test declarations, most of them run
# weekly (`#[ignore]`): they are not code to cover and would only dilute the
# line count.
ARGS=(--workspace --all-features --lcov --output-path "$OUT_DIR/lcov.info"
    --ignore-filename-regex 'desk_sim/fluidity_matrix\.rs')
if [[ "${1:-}" == "--check" ]]; then
    threshold="${2:?usage: scripts/coverage.sh [--check <percent>]}"
    cargo llvm-cov "${ARGS[@]}" --fail-under-lines "$threshold"
else
    cargo llvm-cov "${ARGS[@]}"
fi

echo "coverage report: $OUT_DIR/lcov.info"
