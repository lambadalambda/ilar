# Subagent paths share their plumbing

## Summary

subagent.rs duplicates its own plumbing across the
foreground/background/notification paths:

- `child_spawner` and `route_notification` hand-clone all ~20
  `SubagentSpawner` fields into a new `Self` (268-297 vs 1172-1195);
  a new field needs three sites or silently diverges.
- Child registry construction and system-prompt assembly are
  duplicated between `run_task_observed` and `route_notification`,
  with a third prompt copy in `RuntimePlan::resolve`.
- The lease-acquire → revalidate → run → outcome-mapping pipeline
  exists twice (background 685-780/849-904 vs foreground
  923-951/1012-1032), and the background copy repeats its
  "Task cancelled" notification block four times verbatim.

`run_task_observed` is ~730 lines mixing validation, session
lifecycle, leasing, registry, spawning, watchdog, and formatting —
the split is the direct cause of the duplication.

## Requirements

- One spawner-derivation helper, one registry/prompt builder, one
  shared run pipeline parameterized by output type.

## Acceptance Criteria

- Existing subagent/background tests pass unchanged.

## Milestone

12 — Health sweep
