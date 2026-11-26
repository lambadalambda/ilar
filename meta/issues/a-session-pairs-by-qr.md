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

## Outcome

Done 2026-11-26. `crates/ilar-remote` has the ticket, the pairing rule
and the host (13 tests, two of them mutation-checked); `/remote` in the
TUI starts a host per session and shows the code (`remote.rs`, 6
tests). Live on tenco: the `pair` example paired with a real TUI
through n0's relay in about 1 s, and zbarimg decoded the link from the
captured screen. Each start makes a new key, and the host stops when
the session ends, so a code from session A does not reach session B.
No phone was tried; that comes with [[the-remote-pwa]].
