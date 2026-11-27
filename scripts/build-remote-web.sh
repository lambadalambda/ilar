#!/usr/bin/env bash
# Builds the remote control web app (see docs/interface.md, /remote) into
# OUT (default target/remote-web): the wasm client and its JS glue, the
# renderer `ilar serve` and share files use, and the shell in web/remote.
# Needs the wasm32-unknown-unknown target and wasm-bindgen-cli at the
# version crates/ilar-remote-web pins ($WASM_BINDGEN picks the binary).
set -euo pipefail
cd "$(dirname "$0")/.."
OUT="${1:-target/remote-web}"
WBG="${WASM_BINDGEN:-wasm-bindgen}"
PINNED=$(sed -n 's/^wasm-bindgen = "=\(.*\)"$/\1/p' crates/ilar-remote-web/Cargo.toml)
if ! "$WBG" --version 2>/dev/null | grep -qF "$PINNED"; then
  echo "need wasm-bindgen-cli $PINNED: cargo install wasm-bindgen-cli --version $PINNED" >&2
  exit 1
fi

cargo build --release -p ilar-remote-web --target wasm32-unknown-unknown
rm -rf "$OUT"
mkdir -p "$OUT/pkg"
"$WBG" --target web --no-typescript --out-dir "$OUT/pkg" \
  target/wasm32-unknown-unknown/release/ilar_remote_web.wasm

ASSETS=crates/ilar-tui/src/web/assets
cp "$ASSETS/app.js" "$ASSETS/app.css" "$OUT/"
cp web/remote/* "$OUT/"
# A new build gets a new cache name, so phones drop the old shell, and
# the cache list is what was built (wasm-bindgen may add snippets).
VERSION=$(git rev-parse --short HEAD 2>/dev/null || echo dev)
sed -i.bak "s|<span id=\"pair-version\">build dev</span>|<span id=\"pair-version\">build $VERSION</span>|" "$OUT/index.html"
rm "$OUT/index.html.bak"
FILES=$(cd "$OUT" && find . -type f ! -name sw.js | sed 's|^\./||' | sort | sed 's/.*/"&"/' | paste -sd, -)
sed -e "s/^const VERSION = \"dev\";/const VERSION = \"$VERSION\";/" \
  -e "s|^const SHELL = \[\];|const SHELL = [\"./\",$FILES];|" \
  web/remote/sw.js > "$OUT/sw.js"
echo "built $OUT ($(du -sh "$OUT" | cut -f1); wasm $(du -h "$OUT/pkg/ilar_remote_web_bg.wasm" | cut -f1))"
