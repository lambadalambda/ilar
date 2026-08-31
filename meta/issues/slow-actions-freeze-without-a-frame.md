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

## What landed (2026-08-31)

Rewind — the seconds-long one — runs as a joined task: the loop keeps
drawing, notifications pause for the duration (a notification turn
mid-rewind would write the log being rewritten; the writer-exclusion
story was adversarially traced and holds), quit refuses until it
lands, a message typed meanwhile rides the prefill into the reopened
session, and failure restores the pause it found.

## What remains

- The model switch's second load re-measured cheaper than filed: the
  first load leaves a warm replay checkpoint, so the reread is a tail
  read and the estimate walk is CPU-bound. Still two walks where one
  would do; fold the estimate into `persist_model_change`'s loaded
  session when touching that seam anyway.
- Fork, the turn picker, and `direct_resume_blocked` still full-load
  their targets inline — one O(log) stall each, user-invoked.
- The list-mode session picker's `store.list()` is inline but
  head-reads only; visible only with hundreds of sessions on a slow
  disk.
- Structural: `switch_blocked`/`observe` do not model a rewind in
  flight — safety rests on layered busy gates; an explicit term would
  make it structural. Esc mid-rewind silently does nothing; a "rewind
  cannot be aborted" notice would be kinder.
