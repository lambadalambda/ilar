# A session pairs by QR

## Summary

Part of [[remote-control-over-iroh-index]]. A TUI command starts remote
control for the current session: a new iroh endpoint with a fresh key
for this session only, and a QR code with the ticket.

## Requirements

- The ticket holds the endpoint address and a one-time pairing secret.
- The first peer that shows the secret is pinned by endpoint id; every
  other peer is refused. The secret is spent after one use.
- Remote control stops (endpoint closed) when the session ends or the
  user stops it. The key is not stored, so an old QR code is dead.

## Acceptance Criteria

- A peer without the secret, or a second peer after pairing, is
  refused (tested).
- A QR code from session A does not reach session B.
