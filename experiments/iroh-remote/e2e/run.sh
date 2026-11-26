#!/usr/bin/env bash
# Host + static server + headless Chrome, then clean up. Needs node and
# CHROME (path to a chrome binary); run after the wasm build in README.md.
set -u
cd "$(dirname "$0")/.."
cargo build --quiet --release --bin spike-host
./target/release/spike-host http://127.0.0.1:8080/ > /tmp/spike-host.out 2> /tmp/spike-host.err &
HOST=$!
python3 -m http.server 8080 --bind 127.0.0.1 --directory web > /tmp/spike-http.log 2>&1 &
HTTP=$!
for _ in $(seq 1 60); do grep -q '^http' /tmp/spike-host.out && break; sleep 0.5; done
LINK=$(grep '^http' /tmp/spike-host.out)
echo "link length: ${#LINK}"
node e2e/drive.mjs "$LINK"
echo "--- host stderr"
cat /tmp/spike-host.err
kill $HOST $HTTP 2>/dev/null
true
