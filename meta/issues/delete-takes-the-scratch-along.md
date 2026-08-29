# Delete takes the scratch along

## Summary

`delete()` removes the log, index, ids and lock — not `{id}.live`
(store.rs:412-425). A crash-leftover scratch survives its session's
deletion for up to 24h, and scratch-watching readers (serve, the
focus view to come) see an active turn for a session that no longer
exists.

## Fix

One `remove_file(live_path(..))` inside the held lease.

Size: S. Source: sweep 2026-08-29, store.
