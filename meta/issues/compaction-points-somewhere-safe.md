# Compaction points somewhere safe

## Summary

- After a warm-cache compaction the standing notice says "compacted
  automatically to keep the provider cache warm — /rewind reopens
  the full context" (schedule.rs:246-251, 432-435). Every rewind cut
  is a user message and the newest row arms as "↵ drops 1, restores
  tree" (modals.rs:1917-1921): following the advice discards the
  last answer and reverts its edits. Point at the `history` tool
  ("nothing is lost; the full log stays searchable") or provide a
  real un-compact.
- An aborted compaction sets `Activity::Paused` with status
  "compaction aborted" (schedule.rs:263-269), so the status line
  reads `Ⅱ compaction aborted` until the next message where an
  aborted turn reads `■ aborted`.

Size: S. Source: UX sweep 2026-09-15, session lifecycle.
