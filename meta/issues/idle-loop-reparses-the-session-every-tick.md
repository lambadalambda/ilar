# The idle loop re-parses the session on every tick

## Summary

The top of `run_app`'s loop calls `store.load(session_id)` — a full
JSONL parse of the current session — on every iteration where no turn
runs, to detect a pending question stranded by a failed or cancelled
resume. That is four parses per second while idle and one per
dispatched input event; on a long session it puts a full-file parse
between every keystroke and its frame.

## Requirements

- The stranded-question check runs only when it can change anything:
  once at startup and once after a turn completes — not on every
  iteration.
- Behavior is otherwise unchanged: a pending question left behind by
  a failed resume still reopens its modal.

## Acceptance Criteria

- No `store.load` in the steady-state idle path (verified by reading
  the loop; the existing pending-question tests still pass).
- Typing latency no longer scales with session length while idle.

## Milestone

11 — Beyond the terminal

## Outcome

The check is gated on a `recheck_pending_question` flag: armed at
startup and at the turn-join site (the only two moments a stranded
question can newly exist), consumed only when no turn runs and no
question modal is open. The steady-state idle path no longer touches
the store; the existing pending-question tests pass unchanged.
