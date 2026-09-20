# An append can write an unloadable log

## Summary

`Session::append` accepts any event. `validate_replay` — which runs on
every `load`, including `acquire_writer().load()` — rejects a log where
an ordinary event sits between a tool call and its result:

    session <id>: new event before tool calls received results

So a caller that appends a `UserMessage` while a call is unanswered
writes a file that no later open can read. The append returns `Ok`.
The failure is silent until the next open, and permanent.

Confirmed empirically: append an assistant message carrying an
unanswered `question` call, then a user message, then `store.load` —
the load fails with the message above.

Three callers guard against this by hand, each differently:

- `agent/turn.rs` refuses a turn when `pending_question().is_some()`.
- `store.rs::rewind_target` refuses a rewind for the same reason.
- `ilar-tui/src/main.rs::record_salvage_in` refuses to write a
  salvaged result for the same reason (2026-09-20).

`pending_question()` is the wrong predicate in general: it returns
`None` when *more* than one call is unanswered, which is a worse state,
not a safer one. It happens to be sufficient for a session obtained
through `SessionWriter::load`, because that resolves every unanswered
call except a pending question — but nothing says so at the call sites,
and a fourth caller will get it wrong.

## Requirements

- `Session::append` refuses an event the replay validator would reject,
  rather than writing it. An `Err` the caller can handle beats a file
  nobody can read.
- The three existing hand-rolled guards either go away or become
  redundant rather than load-bearing.

## Acceptance Criteria

- A test appends a `UserMessage` to a session with an unanswered tool
  call and gets an `Err`; the log still loads afterwards.
- `SessionWriter::load`'s own appends (the interrupted-call
  `ToolResult`s) still work — they are the legal case.

## Notes

- Found reviewing the salvage-persistence change, 2026-09-20.
- Size: M. The invariant is `validate_replay`'s; the question is how to
  expose it to `append` without re-validating the whole log per event.
  Tracking unanswered call ids on `Session` is the obvious shape, but
  `rewind_to` and the checkpoint restore mutate `events` directly and
  would have to keep it in step.
