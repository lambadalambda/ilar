# iroh reaches a browser

## Summary

Spike for [[remote-control-over-iroh-index]]: prove that a native iroh
endpoint and an iroh endpoint compiled to WASM in a browser page can
exchange JSON through n0's public relay, before we build a protocol on
it.

## Requirements

- `spikes/iroh-remote/`, outside the cargo workspace: a native binary
  that binds an endpoint, prints its address as a ticket (and a QR code
  in the terminal), and answers JSON lines on a custom ALPN.
- A WASM build (wasm-bindgen) with a small HTML page that takes the
  ticket, dials, sends a JSON message and shows the reply.

## Acceptance Criteria

- The page in headless chromium on tenco connects to the native side
  and gets the echo back, through the relay.
- DEVLOG records: WASM bundle size, time to connect, and what broke.

## Notes

- Pin iroh 1.4.1 (1.4.2 and 1.5.0 are less than two weeks old).
