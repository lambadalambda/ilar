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
