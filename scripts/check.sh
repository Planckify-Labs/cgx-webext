#!/usr/bin/env bash
# Runs every check that needs no network. This is what CI runs.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

step() { printf '\n==> %s\n' "$*"; }

step "rustfmt";  cargo fmt --all --check
step "clippy";   cargo clippy --workspace --all-targets -- -D warnings
step "tests";    cargo test --workspace
step "shim";     node --test Shim/test/shim.test.js Shim/test/store.test.js
step "ASan";     scripts/asan-smoke.sh
step "coverage doc is up to date"
cargo run -q -p webext-cli -- registry --markdown | diff -u docs/COVERAGE.md - && echo "docs/COVERAGE.md is current"
printf '\nAll checks passed.\n'
