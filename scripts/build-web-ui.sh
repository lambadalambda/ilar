#!/usr/bin/env bash
# Builds the session renderer (web/ui, Svelte) into the app.js and
# app.css that the binary embeds. The output is committed, so cargo
# never needs Node; run this after a change under web/ui.
#
#   scripts/build-web-ui.sh           build into crates/ilar-tui/src/web/assets
#   scripts/build-web-ui.sh --check   fail if the committed build is stale
set -euo pipefail
cd "$(dirname "$0")/../web/ui"
ASSETS=../../crates/ilar-tui/src/web/assets
[ -d node_modules ] || npm ci --no-audit --no-fund
npm test --silent
if [ "${1:-}" = "--check" ]; then
  OUT=$(mktemp -d)
  trap 'rm -rf "$OUT"' EXIT
  npx vite build --logLevel error --outDir "$OUT"
  for file in app.js app.css; do
    cmp -s "$OUT/$file" "$ASSETS/$file" || { echo "$file is stale: run scripts/build-web-ui.sh" >&2; exit 1; }
  done
  echo "web/ui build is fresh"
else
  npx vite build --logLevel error
  echo "built $ASSETS/app.js and app.css"
fi
