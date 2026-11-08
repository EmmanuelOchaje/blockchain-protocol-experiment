#!/usr/bin/env bash
# One command that checks every deliverable of the EVM project.
#
#   1. cargo build --release           (deliverable 1)
#   2. cargo clippy, warnings = errors (deliverable 1)
#   3. cargo test --workspace          (deliverables 2, 3 and 4)
#   4. scripts/e2e-cast.sh             (a live node driven by Foundry's cast)
#
# Step 4 needs `cast` and `jq`. Without them it is skipped with a note, and the
# Rust tests still cover the same behaviour through the JSON-RPC client.
#
# Usage: scripts/verify.sh   (from toy-node/)

set -euo pipefail
cd "$(dirname "$0")/.."

banner() { printf '\n######## %s\n' "$*"; }

banner "1/4 cargo build --release"
cargo build --release

banner "2/4 cargo clippy --workspace --all-targets (warnings are errors)"
cargo clippy --workspace --all-targets -- -D warnings

banner "3/4 cargo test --workspace"
# Trust cargo's exit code, not a grep: one test is named "..._not_panicked_on".
if ! cargo test --workspace >/tmp/breeja-tests.log 2>&1; then
  grep -E '^test .*FAILED|panicked at' /tmp/breeja-tests.log >&2 || true
  echo "tests failed; full output in /tmp/breeja-tests.log" >&2
  exit 1
fi
grep -E '^test result' /tmp/breeja-tests.log
total=$(grep -E '^test result' /tmp/breeja-tests.log | sed -E 's/.* ([0-9]+) passed.*/\1/' | paste -sd+ | bc)
echo "all $total tests passed"

banner "4/4 live end-to-end with cast"
if command -v cast >/dev/null && command -v jq >/dev/null; then
  scripts/e2e-cast.sh
else
  echo "SKIPPED: install Foundry (https://getfoundry.sh) and jq to run scripts/e2e-cast.sh"
fi

banner "verification complete"
