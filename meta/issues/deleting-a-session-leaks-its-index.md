# Deleting a session leaks its index

## Summary

`SessionStore::delete` (store.rs:417-427) removes
`{id}.replay.json`, `{id}.jsonl`, and `{id}.lock`, but the replay id
index written by `publish_checkpoint` is named
`{id}.replay.{generation}.ids` (store.rs:1190) — delete neither
reads the checkpoint for the generation nor globs for the pattern.
Every deleted session that ever compacted leaves an orphaned `.ids`
file in the session root forever.

## Requirements

- Delete removes the session's id-index files (glob
  `{id}.replay.*.ids`).

## Acceptance Criteria

- A test: delete after a compaction leaves no files bearing the
  session id.

## Milestone

12 — Health sweep

## Outcome

`delete` now scans the store root for `{id}.replay.*.ids` (full
literal prefix, ok-if-absent) and unlinks them under the writer
lease — covering current and crash-stranded generations.
`publish_checkpoint` was verified to already clean the previous
generation on republish, so the scan closes the only remaining
path. Pinned by
`delete_removes_replay_id_indexes_of_every_generation`, which
plants a stranded generation and asserts nothing bearing the id
survives while a neighbour session still loads. (7ee03cf)
