# The remote wire

## Summary

Part of [[remote-control-over-iroh-index]]. The protocol on the paired
connection: session events out, user input in.

## Requirements

- Out: a history snapshot on connect, then live events (text and
  thinking deltas, tool lifecycle, approvals and questions, turn state)
  as serializable DTOs, not `LoopEvent` (it carries `Instant`).
- In: prompt, approval answer, question answer, cancel.
- A reconnect resumes from the snapshot; no event log on the phone.
- Versioned ALPN (`ilar/remote/1`).

- `HostEvent::Paired`/`Welcome` hand the `Connection` to the session,
  which runs the wire; `serve` in `ilar-remote/src/host.rs` then ends
  after the handshake.
- The pinned peer's newest connection wins; the older one is closed (a
  phone that changes network reconnects before the old one times out).
- Bound unpaired handshakes (a semaphore of about 8) and report at most
  one `Refused` per peer, so a peer that knows the id cannot flood the
  TUI.

## Acceptance Criteria

- Wire round trips are tested in-process (two native endpoints).
- The events come from the same fold the TUI uses where one exists.
