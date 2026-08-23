# Rewind a session in place

## Summary

Rewind conversation and working tree together to an earlier user turn,
in the same session, without breaking the append-only session log. The
model for this already exists: like `Compaction`, a rewind is an
appended marker event that replay honours — compaction drops the head,
rewind drops the tail.

## Requirements

- New `SessionEvent::Rewind { id, to, tree_restored, tree_saved, ts }`:
  `to` is the canonical event index the session is cut back to;
  `tree_restored` is the checkpoint commit the working tree was reset
  to (absent when the turn had no snapshot); `tree_saved` is a fresh
  safety snapshot of the tree taken immediately before restoring, so a
  rewind is itself recoverable.
- Replay folds rewinds out: loading a session whose log ends
  `…events…, Rewind { to }` behaves exactly as if the log were
  `events[..to]`. Multiple rewinds fold iteratively. Both the indexed
  fast path and the full-parse fallback agree.
- `SessionStore::audit_events` still returns every line, including the
  abandoned tail and the rewind markers themselves.
- A valid cut is a `UserMessage` that starts a turn, with no unanswered
  tool calls before it — the same boundary `validate_replay` already
  enforces between calls and results. Invalid cuts are rejected.
- The rewind operation takes the writer lease, validates the cut,
  restores the tree (when a checkpoint exists for that turn), appends
  the event, and leaves the session loadable with `validate_replay`
  passing.
- Tree restore: make the working tree match the snapshot — overwrite
  changed files, recreate deleted ones, delete files that did not exist
  in the snapshot — while never touching ignored files, the user's
  index, HEAD, or the branch. Warn (do not fail) when HEAD has moved
  since the checkpoint.
- A compaction event before the cut keeps working; one after the cut is
  folded away with the rest of the tail.

## Acceptance Criteria

- Tests: append a rewind, reload through both replay paths, and the
  events and transcript equal the truncation; `audit_events` shows the
  full log; a second rewind past the first folds correctly.
- A rewind interleaved with compaction (before and after the cut)
  replays correctly.
- Scratch-repo tests prove the tree restore semantics above, including
  the safety snapshot and the ignored-files exclusion.
- Rejection tests: cut inside a tool-call/result pair, cut at a
  non-user-message index, rewind while another writer holds the lease.
- The full suite passes.

## Notes

- Killing session services on rewind is correct: they may have been
  started after the target point. The TUI reuses the session-switch
  rebuild path, which already drops them.
- Sessions recorded before checkpointing existed rewind conversation
  only; `tree_restored` stays empty.

## Outcome

Landed as designed, plus review-driven hardening: the writer lease is
held from validation through the marker append (an active turn rejects
the rewind before any git work); the rewind target is verified by event
id under the lease; raw `Rewind` appends are rejected so only
`rewind_to` can write markers; the replay index is dropped *before* the
marker lands, so no crash point leaves a stamp-valid pre-rewind index;
`restore` tolerates file/directory type conflicts; and both compaction
cut policies now keep a turn's checkpoint inside the kept window
(cutting between them silently degraded a later rewind to
conversation-only). Known cosmetic leftovers: the session listing
titles from the raw file head, so a rewound-away first message can
still title the list entry; `physical_line_count` in the replay index
undercounts after a rewind (only mis-numbers parse-error messages).

## Milestone

8 — Time travel
