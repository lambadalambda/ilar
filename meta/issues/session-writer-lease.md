# Session writer lease

## Summary

Independent session handles can run concurrent turns and interleave stale snapshots into one append-only log.

## Requirements

- Introduce a validated filename-safe `SessionId` before deriving session or lock paths.
- Hold one exclusive writer lease for the full load-turn-append transaction.
- Cover root turns, resumed tasks, compaction, subagents, and separate ilar processes.
- Define contention as a clear rejection rather than an indefinite wait.
- Keep read-only inspection available while a writer owns the session.
- Make inspection ignore an in-progress trailing record without repairing it while leased.

## Acceptance Criteria

- Two in-process turns cannot mutate one session concurrently.
- A second process receives an actionable busy-session error.
- Cancelling or crashing a writer releases the lease.
- Compaction does not open a conflicting independent writer.
- CLI and model-supplied task IDs cannot traverse outside the session root.
