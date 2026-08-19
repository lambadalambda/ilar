# Manual compaction + summary visibility

## Summary

Compaction is automatic-only (85%) and the live "transcript compacted"
line hides the summary that restored sessions show.

## Requirements

- Palette entry "Compact session" triggers compaction without waiting
  for the threshold (immediately if feasible, otherwise forced on the
  next turn with a clear notice).
- The live Compacted event carries the summary; the transcript line
  shows it like restored sessions do.

## Acceptance Criteria

- Tests: forced compaction path; summary present in the live line.
