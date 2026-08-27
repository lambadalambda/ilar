# Advisory read leases

## Summary

Approved 2026-08-28 from a screenshot: a model backgrounded two
explores (correctly), then its next bash — a /tmp probe that never
touched the checkout — queued behind their read leases until both
finished, because bash is Mutating and Mutating takes the write
side of the workspace RwLock. Background-by-default's promise
("frees the parent to keep working") collided with the lease
design. Decision, after weighing worktrees-for-readers (can't see
uncommitted work), snapshot views (real feature, separate issue)
and Claude Code's lock-free reality: reads become advisory.

New rule, one sentence: mutable work runs in worktrees and never
collides; read-only work runs in place, sees everything, blocks
nothing, and accepts that the tree may shift while it looks.

## Requirements

- ReadOnly workspace acquisition stops taking the read lock:
  readers never wait and never make anyone wait. Mutating keeps the
  exclusive write lock — two mutators in one checkout stay
  impossible, which is the guarantee whose violation is
  unrecoverable. The edit gate remains the stale-write protection.
- Background read-only children stop inheriting the parent's lease
  (an Arc'd write permit outliving the parent task) and stop being
  demoted for one: BACKGROUND_DEMOTED_BY_LEASE and the explicit
  "cannot outlive a parent workspace lease" error go away —
  the situations they guarded no longer exist.
- The one wait that remains (a mutating tool while a same-checkout
  mutable task runs) explains itself: the executor reports
  "waiting for the workspace — a mutable task holds it" through
  the live tail, so "queued" is never a mystery again.
- Docs and schema text that describe the old demotion or the old
  blocking are updated.

## Acceptance Criteria

- A held ReadOnly lease does not block a Mutating acquisition and
  vice versa; Mutating vs Mutating still excludes. Demotion-by-
  lease paths removed with their tests flipped; the waiting notice
  reaches the tool tail.

## Milestone

13 — Guard rails

## Outcome

`WorkspaceScheduler::acquire` returns a free permit for ReadOnly —
the `WorkspacePermit::ReadOnly` variant is gone, and a new
`try_acquire` backs both `try_acquire_lease` and the executor's
waiting notice. The BY_LEASE demotion block, its const, and the
"cannot outlive a parent workspace lease" error are deleted;
background children stop inheriting the parent's lease (the Arc'd
write permit would outlive the parent task — they take their own
free read lease instead). The executor tries the permit first and
reports "waiting for the workspace — a mutable task holds it"
through the live tail before the blocking acquire, so the one
remaining wait (writer vs writer) names itself under the queued
row. Tests: a scheduler unit test pins the one-sentence rule;
`leased_child_rejects_background_task` became
`leased_child_detaches_a_background_reader`, and the foreground
demotion test became
`a_leased_parent_detaches_a_defaulted_read_only_task`. Docs state
the rule and the drift bargain. Follow-up left open by design:
checkpoint-materialized snapshot views for readers that need strict
consistency — nono-style CAS restore, which ilar's checkpoint trees
already implement.
