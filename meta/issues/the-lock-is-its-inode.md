# The lock is its inode

## Summary

`acquire_writer_id` flocks `{id}.lock` without re-checking that the
fd's inode still matches the path (store.rs:269-301). Around
`delete()`'s unlink, a waiter can win the lock on an orphaned inode
while a third process locks a fresh file at the same path — two
processes both believing they own the session.

## Fix

Classic re-stat-after-lock: compare fd dev/ino with the path's,
retry on mismatch.

Size: S. Source: sweep 2026-08-29, store.
