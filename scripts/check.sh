#!/usr/bin/env bash
# The quality gate: what "clean" means for this repository. Run from the
# workspace root before a substantial commit, or point CI at it.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
