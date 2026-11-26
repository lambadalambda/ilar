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

## Acceptance Criteria

- Wire round trips are tested in-process (two native endpoints).
- The events come from the same fold the TUI uses where one exists.
