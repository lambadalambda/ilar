# Fork a session at a point

## Summary

`SessionStore::fork` copies a whole session under a new id. Add
`fork_at(id, cut)`: the same, truncated to an earlier turn boundary —
the non-destructive sibling of rewind. Try two approaches from the same
context and keep the winner.

## Requirements

- `SessionStore::fork_at(&self, id, cut) -> io::Result<String>` copies
  the active replay window's `events[..cut]` verbatim under a fresh
  session id, rewriting only `Meta.session_id`, exactly as `fork` does.
- Valid cuts are the same boundaries rewind accepts: a turn-starting
  `UserMessage` with no unanswered tool calls before it. Invalid cuts
  are an error, not a silent adjustment.
- `fork_at` with `cut == events.len()` is equivalent to `fork`;
  implement one in terms of the other rather than duplicating the copy
  loop.
- The fork loads cleanly: `validate_replay` passes and the transcript
  equals the truncated original's.
- The fork does not copy or move tree checkpoints; its
  `refs/ilar/checkpoints/<new-id>` chain starts fresh on its first
  turn. Rewinding within the fork before that first turn is
  conversation-only, matching pre-checkpoint sessions.

## Acceptance Criteria

- Tests: fork at a mid-session boundary → loads, validates, transcript
  equals the truncation; full-length cut produces the same events as
  `fork`; cut between a call and its result is rejected; cut on a
  non-boundary index is rejected.
- Forking a compacted session at a post-compaction boundary keeps the
  summary and replays correctly.
- The full suite passes.

## Notes

- Same caveat as `fork`: the copy is of the active window, so
  pre-compaction history does not travel.
- Child-session references in the dropped tail simply do not appear in
  the fork; the child JSONL files remain owned by the original session.

## Milestone

8 — Time travel
