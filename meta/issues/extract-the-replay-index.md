# Extract the replay index

## Summary

store.rs (1,939 lines) is five subsystems in one file: session
CRUD/listing, JSONL parse+validation, the replay checkpoint cache
with stamp-race detection, a complete ~290-line Merkle-tree paged id
index with proofs (`ReplayIdIndex`, `write_id_records`,
`merkle_level_counts`), and transcript rendering. The Merkle index
has zero session semantics and would extract cleanly.

Two adjacent nits found in the same sweep: `SessionStore::rewind`
has no production callers (production goes through
`rewind.rs::rewind_session`, which adds the `target_user_id` guard
the store method lacks — delete it or fold the guard in), and the
checkpoint's `physical_line_count` is set from the *folded* event
count (store.rs:1725), so tail-parse diagnostics report wrong line
numbers after any rewind.

## Requirements

- Move the Merkle index + checkpoint machinery to
  `session/replay_index.rs` (or similar).
- Remove or guard `SessionStore::rewind`; fix `physical_line_count`
  to count physical lines.

## Acceptance Criteria

- Existing store/replay tests pass; a diagnostics test pins the
  correct line number after a rewind.

## Milestone

12 — Health sweep
