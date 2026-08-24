# Aborted subagent rows keep spinning

## Summary

Abort a turn while a subagent is working and its row keeps its
spinner: the transcript claims work is in progress that has already
stopped.

The turn's own sweep on `TurnDone { Aborted }` only walked top-level
lines and only touched `state`. Two things defeat it. A subagent's
tool rows are nested in the agent row's `child_lines`, which the sweep
never reached. And the agent row renders `Running` whenever
`child_running` is set, whatever its own state — so even the row it
did mark `Failed` still spun.

`child_running` is cleared by a subagent `TurnDone` activity that
never arrives: aborting drops the parent's tool futures, which
cancels the child mid-run, so it never reports back. Nothing else was
ever going to close those rows.

## Requirements

- An aborted turn closes every open tool row at any depth, and clears
  `child_running` so a closed row renders closed.
- Still self-correcting: a later activity event for work that is
  genuinely alive sets `child_running` again.

## Acceptance Criteria

- A test drives a subagent with a running child tool, aborts, and
  asserts the agent row is `Failed`, no longer claims a running child,
  and has no running child rows.
- The full suite passes.

## Notes

- Visual only. The work really has stopped — the executor drops
  running tool futures on cancel — and the session log is correct,
  because the `ToolResult` is appended before the event that announces
  it. Only the live view was stale, and it is right again on resume.
- The event is dropped rather than delayed: `publish` is biased on the
  cancel token, so a `ToolFinished` racing an abort is discarded. The
  sweep is what closes those rows, which is why it has to be complete.

## Outcome

The sweep is recursive and clears `child_running`. Reproduced by a
test first: an agent row with a running child tool, aborted, then
asserted closed at both levels.

## Milestone

10 — Everyday polish
