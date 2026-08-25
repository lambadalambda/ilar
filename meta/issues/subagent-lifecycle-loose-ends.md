# Subagent lifecycle loose ends

## Summary

Four related teardown gaps in subagent.rs / turn.rs:

1. Cancelling or stall-killing a *background* subagent drops the
   `run_turn` future inside `tokio::select!` (subagent.rs:820-834)
   instead of awaiting it under the cancelled token like the
   foreground path does — the graceful-abort path (persist partial
   deltas, publish terminal) never runs.
2. The session-create rollback (subagent.rs:556-572) deletes with a
   bare `remove_file` instead of `store.delete()`, leaking the
   `.lock` file and any replay index, and bypassing the lease that
   delete documents as required.
3. `route_notification`'s `WouldBlock` loop (subagent.rs:1285-1297)
   retries every 25 ms forever with no backoff or cap — a
   long-held lock spins at 40 attempts/s instead of requeueing like
   every other transient failure there.
4. The provider-error path in turn.rs ends with `bail!` and never
   publishes the reserved terminal `TurnDone` (turn.rs:1518); every
   caller compensates by synthesizing it from the join result. The
   abort-path `ToolFinished` publishes are dead (biased select takes
   the cancelled arm first).

## Requirements

- Background cancel awaits the turn under the cancelled token.
- Rollback uses `store.delete()`.
- `WouldBlock` gets a cap and falls back to requeueing.
- The error path publishes the terminal event; the dead publishes go.

## Outcome

All four gaps closed: background cancel/stall now awaits the turn
under a 5s `BACKGROUND_ABORT_GRACE` (and `shutdown` joins children
concurrently, bounding quit to one grace); the rollback uses
`store.delete` via `rollback_created_session`; `WouldBlock` caps at
~3s then requeues like the other transient paths; and
`persist_failed_step` publishes the terminal `TurnDone { Aborted }`
for all three bail sites, with the dead abort-path `ToolFinished`
publishes removed. Callers' synthesized-TurnDone fallbacks kept:
`run_turn` can still error before the event channel exists.
Known residuals (deliberate): a lease held >3s now surfaces as a
paused notification instead of waiting silently; a turn finishing
inside the grace window still reports cancelled; an append failure
inside `persist_failed_step` can still skip the terminal event.

## Acceptance Criteria

- Tests: background cancel persists partial content and leaves no
  lock file; a failed-create rollback leaves no session files.

## Milestone

12 — Health sweep
