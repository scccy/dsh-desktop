#!/usr/bin/env bash
# Runs the same gates as .github/workflows/ci.yml, in the same order.
# Usage: ./scripts/check.sh   (any failure exits non-zero because of `set -e`)
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

step() {
    printf '==> %s\n' "$*"
    "$@"
}

step cargo fmt --all --check
step cargo clippy --workspace --all-targets -- -D warnings
step cargo test --workspace

printf 'All checks passed.\n'
