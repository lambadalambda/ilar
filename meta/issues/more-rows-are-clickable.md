# "… more" rows are clickable

## Summary

An expanded tool result that truncates ends in a muted "… more" row —
which does nothing when clicked; the user must know to click the tool
header again. A collapsed thought's "… N more line(s) — click to
expand" row has the same defect: it *says* click and carries no click
target. The affordance should be where the eye lands.

## Requirements

- The "… more" row of truncated tool details (args, result, tail,
  diff) carries the tool's click target, so clicking it advances the
  expansion exactly like clicking the header.
- A collapsed thought's preview rows and its "more line(s)" row all
  carry the thought's target; expanded thoughts keep the header-only
  target so a stray click on a wall of text does not collapse it.
- Hover underline picks these rows up automatically (it keys off the
  hit target).

## Acceptance Criteria

- Tests: the truncated detail row carries the tool target and the
  full-state rows carry none; collapsed-thought preview rows carry the
  thought target, expanded rows do not.

## Milestone

11 — Beyond the terminal

## Outcome

`labeled_rows` threads a `more_target` down from the tool: the
"… more" row of truncated args/result/tail/diff carries the tool's
hit target, so clicking it advances the expansion exactly like the
header (and hover-underlines automatically). Collapsed
notification rows (Task/Job) now target every preview row including
the "click to expand" hint; expanded bodies stay bare so a stray
click cannot collapse them. Collapsed thoughts were already a single
targeted header row. Pinned by row-target tests for both shapes.
