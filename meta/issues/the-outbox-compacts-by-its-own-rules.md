# The outbox compacts by its own rules

## Summary

`pending()` compacts via read-filter-rename (outbox.rs:157-196) —
the exact hazard `try_retire`'s tombstone comment forbids: in the
documented two-process degraded mode, one process's compaction can
erase an entry the other recorded between read and rename. Silent
loss, strictly worse than the admitted double-delivery window.

## Fix

Compact through the tombstone (retire) mechanism only, or take the
retired-sidecar approach for pending's rewrite too (append-only
both sides, rewrite only under an exclusive lock).

Size: M. Source: sweep 2026-08-29, subagent/outbox.

## Outcome

One flock file for the outbox directory — never deleted, so it cannot
be the inode race it exists to prevent — taken by `record`, `retire`,
and the read-filter-rewrite inside `pending`. The session-log reads
that decide *what* is delivered stay outside it: holding an exclusive
lock through a replay and an ancestry walk would park every publishing
child for the length of an adoption. A publish landing between the
delivery check and the lock is kept as undelivered, which is the
double-delivery this module already admits to rather than the silent
loss it refuses.
