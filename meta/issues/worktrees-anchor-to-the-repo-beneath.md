# Worktrees anchor to the repo beneath

## Summary

Carried from the wave-orchestration sessions: a session whose cwd
is a directory *above* its repositories (e.g. `~/repos`) cannot
run mutable tasks in worktrees of a repository beneath it. Worse,
the validation error blames the wrong path: it fails on the parent
cwd but names the child path the task asked for, which cost one
orchestrating agent four failed attempts and its parallelism
before it gave up and serialized.

## Requirements

- A task's worktree request resolves against the repository
  containing the *requested* path, not the session cwd: cwd
  `~/repos`, task path `~/repos/project` → worktree of `project`.
- The validation error names the path it actually examined and
  what it expected to find there.
- A test with a session rooted above two repositories running
  mutable tasks in worktrees of each, concurrently.

## Milestone

13 — Guard rails
