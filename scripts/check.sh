#!/usr/bin/env bash
# The quality gate: what "clean" means for this repository. Run from the
# workspace root before a substantial commit, or point CI at it.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
# The core with its default features, the TUI with every feature: the
# same coverage as one workspace-wide all-features run, minus an
# interaction that made a serve test starve only under that invocation
# (meta/issues/an-adoption-test-hangs-once-in-ten.md, 2026-09-05).
cargo test --workspace
cargo test -p ilar-tui --all-features
# The committed Svelte build matches its sources (needs Node; skipped,
# loudly, where there is none).
if command -v npm >/dev/null; then
  scripts/build-web-ui.sh --check
else
  echo "warning: no npm, web/ui build freshness not checked" >&2
fi
