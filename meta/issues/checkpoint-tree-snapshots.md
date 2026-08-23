# Snapshot the working tree at each turn

## Summary

To rewind a session, the working tree at each user turn must be
recoverable. Take a shadow git snapshot when a root-session turn starts
and record it in the session log, without ever touching the user's
HEAD, branch, index, or ignored files.

## Requirements

- A `checkpoint` module in the core crate shells out to git following
  the `git_output` pattern in `tools/mod.rs` (scrubbed `GIT_*` env,
  timeout, `kill_on_drop`).
- Snapshot procedure: populate a temporary index (`GIT_INDEX_FILE`)
  with the full working tree — tracked and untracked files, ignored
  files excluded — then `write-tree` and `commit-tree`. The user's real
  index, HEAD, and working tree are never modified.
- Each session's snapshots form a commit chain: the new commit's parent
  is the previous checkpoint commit, and `refs/ilar/checkpoints/<session-id>`
  points at the tip, keeping every snapshot reachable and out of `gc`'s
  reach.
- The snapshot records the repository HEAD at capture time so a later
  restore can warn when HEAD has moved since.
- New `SessionEvent::Checkpoint { id, commit, head, ts }` appended
  immediately before the `UserMessage` in `run_turn_inner`, under the
  writer lease. `transcript_of` and the TUI session view render nothing
  for it.
- Only root sessions checkpoint. Child (subagent) sessions never do;
  the parent's turn checkpoint covers the shared workspace.
- A cwd that is not a git repository, or any git failure, disables the
  snapshot for that turn without blocking or failing the turn. At most
  a diagnostic is surfaced.

## Acceptance Criteria

- Integration tests against a scratch repository prove: modified and
  untracked files are captured; ignored files are not; `git status`,
  HEAD, and the index are byte-identical before and after a snapshot.
- A second snapshot chains onto the first (parent linkage) and the ref
  moves to the tip.
- A non-git directory yields no snapshot and no error.
- `SessionEvent::Checkpoint` round-trips through serde, is ignored by
  `transcript_of`, and passes `validate_replay`.
- A session recorded before this change still loads and replays.
- The full suite passes.

## Notes

- Snapshots live in the repository's object database, not in
  `ILAR_STATE_DIR`. ilar does not garbage-collect them; deleting
  `refs/ilar/checkpoints/<session-id>` makes them collectable. Wiring
  that into `SessionStore::delete` is out of scope (the store cannot
  assume the repository is reachable).
- `git add -A` cost on large repositories is accepted; git's object
  store dedupes unchanged blobs.

## Milestone

8 — Time travel
