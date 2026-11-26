# iroh remote spike

Proof that a browser page can reach a native iroh endpoint through n0's
public relay. Issue: [iroh reaches a browser](../../meta/issues/iroh-reaches-a-browser.md).
Not part of the cargo workspace.

- `spike-host` binds an endpoint, prints a QR code and a link whose
  `#fragment` is the ticket, and echoes JSON (one bi-stream per request).
- `web/index.html` loads the WASM client, dials the ticket from the
  fragment and sends three requests.

## Build and run

```sh
rustup target add wasm32-unknown-unknown
cargo build --lib --release --target wasm32-unknown-unknown
cargo install --root /tmp/wbg wasm-bindgen-cli --version 0.2.130   # match Cargo.lock
/tmp/wbg/bin/wasm-bindgen --target web --out-dir web/pkg \
  target/wasm32-unknown-unknown/release/iroh_remote_spike.wasm
CHROME=/path/to/chrome e2e/run.sh
```

By hand: `cargo run --release --bin spike-host http://<lan-ip>:8080/`,
serve `web/` on that port, and scan the QR code with the phone camera.
The page runs over plain HTTP; only the in-app camera and the service
worker of the real PWA need HTTPS, which GitHub Pages gives.
