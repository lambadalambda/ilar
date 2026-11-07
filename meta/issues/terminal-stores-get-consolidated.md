# Terminal stores get consolidated

## Summary

The gateway's store gets a weekly consolidation pass; terminal stores
never do. Claude Code's "dream" merges duplicates, deletes contradicted
facts and prunes the index after 24 h and 5 sessions.

## Requirements

- Decide the trigger (time and session count, like Claude Code, or on
  demand) and reuse the gateway's weekly prompt where it fits.
- Define project scope, cost limits, approval and cancellation before
  code. An on-demand entry point can come first; this does not require
  a new daemon or copying Claude Code's timing thresholds.
- Reuse [source-backed correction](consolidation-checks-claims-against-sources.md)
  and its evidence access, rather than maintaining a second validation
  algorithm or treating summaries as original evidence.
- Share one project store across worktrees, with at most one maintenance
  run per store. Do not hold a store lock during a model call or overwrite
  an intervening session edit.
- Keep maintenance out of the user's active conversation and leave it
  off when terminal memory is disabled. Decide automatic enablement only
  after write-safety fixes and outcome measurements.
- Keep core and index context bounded; merge duplicates and move detail
  out of core only when the evidence supports the change. Unavailable
  sources must not cause invented verification.

## Acceptance Criteria

- A design note in this file before code records the trigger and access,
  budget, approval and default policy.
- An integration test runs maintenance on an isolated project store and
  verifies correction, deduplication and core bounds without changing
  the active conversation.
- Tests cover two worktrees requesting maintenance, memory disabled,
  cancellation, budget exhaustion and a note changed during a run.
- An unrelated project's memory is neither read nor written.
- A documented invocation reports what changed and what could not be
  verified. Automatic rollout remains gated by [task-outcome results](memory-is-measured-by-later-task-results.md).

## Notes

- Part of [the memory follow-ups](memory-follow-ups-from-the-component-review.md).
- Depends on [shared-store write safety](serialize-shared-memory-writes.md)
  and the source metadata/retrieval work linked from the parent issue.
- The article is evidence for bounded maintenance, not for a mandatory
  schedule or a wiki representation.
