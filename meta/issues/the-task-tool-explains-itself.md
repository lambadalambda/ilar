# The task tool explains itself

## Summary

Store-wide scan (2026-08-26, 660 calls): task fails 22% overall and
39% on first use per session — models learn the workspace model by
crashing into it. Dominant errors: 41× "nested mutable tasks cannot
reuse their parent checkout; use a validated worktree", 27× "Git
workspace validation failed: not a git repository". The schema
never explains the workspace options up front.

## Requirements

- The task tool's schema/description documents the workspace model
  the way the model needs it before the first call: when workspace
  may be omitted, what a mutable nested task requires, the git
  requirement, subagent_type meanings, and one concrete example
  input shape.
- The two dominant errors name the exact corrective input in their
  text (show the shape, not just the rule).
- Investigate auto-provisioning: when a nested mutable task needs a
  worktree and the parent checkout is a git repo, can ilar create
  the validated worktree itself instead of erroring? If safe and
  bounded (cleanup story, naming, same validation), implement —
  that deletes the 41× class instead of documenting around it. If
  it's not safe, say why in the outcome and land the docs/errors
  alone.

## Acceptance Criteria

- Schema text covers the decision tree; error texts carry the
  corrective shape; if auto-provisioning lands, a nested mutable
  task without a workspace succeeds in a git-repo fixture and
  cleans up after itself.

## Milestone

13 — Guard rails

## Outcome

The schema now teaches the decision before the first call: agent
type fixes tools and writability; same checkout = mutable tasks
serialize behind the write lease, each sees the last one's edits,
nothing merges (dependent work); separate worktrees = parallel,
you merge (independent work); the exact `git worktree add`
incantation and input shape inline, with the example naming an
agent configured in THIS install. Both dominant errors quote one
shared corrective shape. **Auto-provisioning: investigated and
rejected** — a mutable task's product is its worktree diff, so an
ilar-created-and-removed worktree silently discards work behind a
success string, retention would mean sweeping uncommitted work on
heuristics, and `worktree add` mutates the parent repo unasked; the
error-plus-recipe is strictly better. The worktree-isolation skill's
contradicting line fixed alongside. Recorded residual: a relative
workspace.cwd canonicalizes against the process cwd, not the
location (pre-existing; works because worktrees are conventionally
siblings).
