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

- One bi-stream per connection, newline-delimited JSON: `Hello` and
  `Welcome` first, then `ToDevice` / `FromDevice` (`ilar-remote/src/wire.rs`).
  `HostEvent::Paired`/`Welcome` hand the TUI a `Link` (two channels and
  a serial).
- The pinned peer's newest connection wins; the older one is closed (a
  phone that changes network reconnects before the old one times out).
- Bound unpaired handshakes (a semaphore of 8; over it, close code 503
  BUSY, not 403) and report at most one `Refused` per 10 s, so a peer
  that knows the id cannot flood the TUI.
- Events are the web projection of the log (`web::view`, as share and
  serve use), plus `LiveDelta`s from the `.live` scratch.

## Acceptance Criteria

- Wire round trips are tested in-process (two native endpoints).
- The events come from the same fold the TUI uses where one exists.

## Outcome

Done 2026-11-26. `ilar-remote/src/wire.rs` has the frames; the host
hands the TUI a `Link`, and `remote.rs` feeds it a snapshot (the
`web::view` projection share and serve use), log appends, `.live`
deltas (serve's `LiveTail`, moved to the core), busy and the open
prompt. Phone commands go through `apply_remote_command` in `main.rs`,
which shares `decide::submit`, `answer_grant` and `abort_turn` with the
keys. Live on tenco through n0's relay: a phone prompt started a real
turn, a second one queued, `/sessions` was refused, cancel answered.
Streaming text deltas were not seen live (the OpenCode key was
expired); the core `LiveTail` test covers them.

Known gaps, each marked in the code: the snapshot is the whole log in
one message (no paging); a question restored from the log is answered
at the terminal only; an unknown `/command` from the phone is refused
by `prepare_prompt` without telling the phone.
