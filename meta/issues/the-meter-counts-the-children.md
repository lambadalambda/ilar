# The meter counts the children

## Summary

The $ amount and token totals count only the root session's own
steps. A child's `StepComplete` reaches the transcript fold — which
reads nothing but the group counter (transcript.rs:1321) — and never
`accrue_usage` (app.rs:1203, root loop events only). On restore, the
children's views *compute* their totals and the caller keeps only
`.lines` (session_view.rs:479). A delegation-heavy day can spend a
large multiple of what the meter admits to.

## Fix shape

- Live: accrue once per child `StepComplete` at the App level (not in
  the folds — the same activity is applied to the root transcript and
  an open focus view). Pricing needs the child's model, which
  `StepComplete` does not carry: either add it to the activity event,
  or remember it per child from `SubagentConfigured`/the roster.
- Restore: sum the child views' already-computed `total_usage`/
  `total_cost` into the parent's instead of dropping them, recursively
  — mind the depth-8 recursion and the compaction cut.
- Display: consider splitting the readout ("own · with tasks") so the
  context meter's session-scoped number is not conflated with spend.
- Out of scope by design: asides are off the record entirely.

Size: S-M. Source: user question 2026-09-01.

## Outcome (2026-09-01)

`StepComplete` carries the model that priced it, set at the one
emission in turn.rs — which also fixed the root mispricing steps
across a mid-turn model change. Live: child and grandchild steps
accrue into `App.task_usage`/`task_cost` in `push_subagent_activity`,
exactly once per activity (retries re-apply only the fold; verified
no double-count through focus, retry, or broadcast lag). Restore:
each child session's whole log counts once, deduped across its
`task`/`task_message` anchor rows — review caught that the anchored
slices missed notification-driven resume turns entirely, whose
synthetic call ids anchor to no row. Display: Σ is the whole bill,
"(tasks N)" named beside it.

Known gaps, recorded not hidden:
- A routed delivery's turn runs with a discarded event sender, so its
  spend accrues only at the next open (whole-log restore), never
  live.
- Descendants behind folded digest middles or the depth-8 cap are not
  loaded, for lines or spend.
- Compaction's summary calls are metered nowhere, live or restored —
  pre-existing.
- One unpriced child model poisons the combined $, hiding a known own
  cost; and a plan-billed root labels the whole Σ "plan" while its
  API-billed children's real dollars are known. The "own · tasks"
  display split the summary suggested would fix both; deferred.
