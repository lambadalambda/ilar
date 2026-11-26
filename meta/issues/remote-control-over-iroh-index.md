# Remote control over iroh — index

## Summary

Control one running ilar session from a phone, with no server we run.
The session shows a QR code; a PWA (hosted on GitHub Pages) scans it
and connects to that session, and only that session, over iroh. The
browser cannot do UDP, so the browser side always goes through an iroh
relay (n0's public relays by default); QUIC/TLS runs end to end between
the two endpoint ids, so the relay sees only ciphertext.

## Subissues

- [iroh reaches a browser](iroh-reaches-a-browser.md) — spike, first
- [A session pairs by QR](a-session-pairs-by-qr.md)
- [The remote wire](the-remote-wire.md)
- [The remote PWA](the-remote-pwa.md)

## Acceptance Criteria

- From a phone on a different network, scan the QR code of a running
  TUI session, see its transcript live, send a prompt, answer a tool
  approval, and cancel a turn.
- A second session on the same machine is not reachable with that QR
  code.

## Notes

- iroh 1.x (1.4.1 as of 2026-11) builds for `wasm32-unknown-unknown`
  (`wasm_browser` cfg): relay-only, no hole punching, no portmapper.
- `ilar serve` (parked behind the `serve` feature) already folds
  session events for a browser; the remote wire should share that fold
  where it can, not fork a third one. Related: [[web-frontend]],
  [[one-fold-for-live-and-restored]].
