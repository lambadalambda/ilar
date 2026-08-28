# A panic eats the permit

## Summary

Found 2026-08-28 by audit of `subagent.rs`. The completion pipeline
is airtight for every *expected* ending — cancellation, stall,
lease failure, iteration limit, error and clean completion each
reach exactly one `publish_reserved_notification`, and the permit
is reserved before anything can fail. But nothing enforces the
exactly-one invariant against abnormal termination:

- The reserved permit lives in a plain `Option<OwnedPermit>` local.
  A panic anywhere in the spawned task body drops it: capacity
  quietly returns to the channel and **no notification is ever
  sent**. The parent waits forever for a task that is gone.
- No `JoinHandle` result is ever inspected — `shutdown` discards
  them, `running_background` only checks `is_finished`, the drop
  guards remove registry rows regardless. A panicked child is
  indistinguishable from one that never existed.
- Amplifier: every mutex in the file is `.lock().unwrap()` (19
  sites). One panic while a lock is held poisons it; every later
  task then panics at its first lock and loses its notification
  too. Three of those unwraps run in `Drop` — panic-during-unwind,
  process abort.

## Requirements

- The permit becomes a self-reporting guard: dropped without an
  explicit send, it emits a "task ended abnormally" notification
  (it holds the parent session id and description; it has all it
  needs). A panic then surfaces as a failed-task notification
  instead of silence.
- Join results get checked somewhere cheap — the registry sweep or
  `shutdown` — so a panicked task at least logs what it was.
- The poison cascade: take `.lock()` results with
  `unwrap_or_else(PoisonError::into_inner)` where the guarded state
  cannot be torn (registries of ids, maps of steers), so one
  panicked task does not take the whole background runtime with it.

## Acceptance Criteria

- A background task whose future panics produces a notification
  saying the task died, and the next background task still works.

## Milestone

13 — Guard rails
