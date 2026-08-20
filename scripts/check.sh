#!/usr/bin/env bash
# Run the same quality gates CI enforces, locally, before pushing.
# Fails the run if any gate fails. Mirrors .github/workflows/ci.yml.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> cargo fmt --check"
cargo fmt --check

echo "==> cargo clippy (deny warnings)"
cargo clippy --all-targets --all-features -- -D warnings

echo "==> cargo test"
cargo test --all-features

echo "==> cargo build (release)"
cargo build --release

echo "All gates passed."
