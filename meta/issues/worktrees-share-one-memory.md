# Worktrees share one memory

## Summary

A terminal session's memory is keyed by the canonical launch
directory (`ilar::memory::dir_for`). Two git worktrees of one
repository are two directories, so each gets its own store and
neither sees the other's notes — and worktrees are how parallel
streams are run here. Claude Code keys its per-project memory so that
worktrees of the same repository share it (vault write-up,
2026-09-18).

## Requirements

- Inside a git repository, the memory key is the repository's common
  directory (`git rev-parse --git-common-dir`, canonicalised; the
  main checkout's `.git`), so every worktree of it resolves to one
  store. Outside a repository, the launch directory as today.
- A subdirectory of a checkout still shares the checkout's memory —
  the common dir is the same — which is a change from today, where a
  subdirectory is another directory as it is for sessions. Said in
  the docs.
- The slug stays readable: the common dir's parent (the main
  checkout's path) is what gets flattened, not `.git`.
- No git subprocess on the hot path: the common dir is read from
  `.git` (a directory, or the `gitdir:` file a worktree has) by hand,
  as the checkpointer already does for its own purposes, or found by
  walking up.
- A store written under the old key is not migrated; the docs say
  where it is.

## Acceptance Criteria

- A test with a repository and a worktree of it: `dir_for` answers
  the same directory for both, and for a subdirectory of the
  checkout; a plain directory beside them gets its own.
- The slug of a worktree names the main checkout.
- docs/sessions.md's memory section says what shares a memory.

## Notes

Follow-up to the memory stream (milestone 22). Small, but it changes
which store an existing checkout opens: once it lands, a memory
written from a subdirectory before it is left behind under its old
slug.

Size: S.
