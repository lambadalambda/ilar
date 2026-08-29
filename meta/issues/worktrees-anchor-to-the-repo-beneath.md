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

## Outcome

The anchor is the repository containing the requested path.
`validated_git_worktree` resolves the request first; a session cwd
inside a repo keeps the old same-repository rule, and a
repositoryless session cwd accepts any repository whose common dir
sits beneath it — the issue's exact case. Every refusal names the
path actually examined and what was expected there, ending the
wrong-path blame that cost an orchestrator four attempts. The
registration check anchors on the requested worktree's root, child
locations key their own locks (two repos under one session run
concurrently — pinned by a barrier provider that deadlocks if they
serialize), and lease revalidation and notification routing funnel
through the same validator.

Deliberate non-change, for a follow-up decision: a session whose
cwd is itself a repo still cannot anchor a different repository
nested beneath that checkout.
