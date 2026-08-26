# Background by default, for readers

## Summary

Approved 2026-08-26, staged deliberately: background subagents free
the parent to keep talking (the "always someone to talk to" UX),
but async orchestration demands model discipline — and the store
scan shows GLM-5.3 failing the task tool 39% on first use with the
*current* options. So: flip the default to background for
READ-ONLY agent types only (independent by nature, no merge burden,
parallel-safe under per-location leases); mutable tasks stay
foreground-default. Then measure with the store-scan method before
widening.

## Requirements

- Read-only agent types default `background: true` when the caller
  doesn't say; mutable types keep foreground. The task schema (just
  rewritten) states the default per type and when to override in
  both directions ("need the result to continue → foreground").
- Depends on talk-to-your-tasks landing first, so a parent can
  steer/collect a background child naturally.
- A measurement note in the issue on completion: re-run the
  first-use failure scan and a parent-idle-time comparison after a
  week of real use; widen or revert based on numbers, not vibes.

## Acceptance Criteria

- Defaulting tests per agent type; schema text pins; the
  measurement plan recorded.

## Milestone

13 — Guard rails

## Outcome

The default follows the agent's workspace mode, resolved once at
input time so every downstream gate sees a plain bool; explicit
values win both ways. Two demotion paths (capacity full; a held
lease the child would outlive) run a *defaulted* background task
foreground with a named note instead of erroring — explicit
background:true keeps the honest errors. The schema and docs teach
both override directions and point at task_message instead of
polling; three existing tests were pinned foreground to stay
non-vacuous. Measurement plan, binding: (1) re-run the first-use
failure scan against the 39% GLM baseline, watching specifically
for capacity-errors and wait/poll behavior on detached results;
(2) parent-idle-time inside task calls, before/after, bucketed by
workspace mode; (3) widen to mutable types only if failures hold
and idle time drops; the demotion note's frequency is the cheap
counter for an undersized notification channel. Residual worth a
look if background readers contend: TaskTool::workspace_access
declares Mutating for every task call. (Committed with the
demotions; fmt drift from the prior rewrites cleaned alongside.)
