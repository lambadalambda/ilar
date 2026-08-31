# Slow actions freeze without a frame

## Summary

A family of user-invoked actions that hold the UI task through O(log)
or subprocess work with no repaint, no spinner, no Esc:

- Rewind awaits the full writer replay plus git snapshot + restore
  inside the key dispatch (main.rs:3371-3378; rewind.rs:36-83) —
  seconds on a big working tree, up to the 60 s git timeout.
- A model switch does two full replays per selection: the writer
  replay in `persist_model_change` and a second load for
  `session_context_tokens` (main.rs:907-935; dispatch 3521, 3557,
  and the per-turn override sites).
- Fork copies the whole validated history synchronously — load,
  serialize, `sync_data` (main.rs:2741, 3226, 3451); the turn picker
  and `direct_resume_blocked` each full-load their target
  (main.rs:2722, 822-830).
- The session picker opens with a synchronous per-file `store.list()`
  head-read (main.rs:3305-3313) — a visible hitch with hundreds of
  sessions on a slow disk.

The content-search modal already shows the fix: background the scan,
stream results, keep drawing (main.rs:2237, 2601). Run rewind as a
joined task like a turn; share one reader across a model adoption;
show a frame before anything that can take a second.

Size: M. Source: sweep 2026-08-31, responsiveness & memory.
