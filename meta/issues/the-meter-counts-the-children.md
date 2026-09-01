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
