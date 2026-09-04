# The status field is shown or gone

## Summary

`app.status` is written in ~25 places — `"retrying provider (2/5)"`,
`"running 3 tools"`, `"task result held — send a message to retry"`,
`"compaction failed"` (app.rs:1083-1401, main.rs:620/928/3993/4029,
schedule.rs:244-285) — and read by nothing: `view.rs::status_line`
renders `self.activity` only. The retry counter and tool count the
loop carefully maintains are never seen. Alongside: three
vocabularies for one state (`tools` in the status line, `processing
tools and agents…` in the activity row, `running {name}` in the dead
field) and a retry notice formatted with Debug `Duration`
(`"provider retry: {error} — in 2.000000001s"`, app.rs:1204).

## Requirements

- Either render `status` in the activity slot when set, or delete the
  field and every write. Pick one; do not leave a write-only field.
- One label per `Activity`, used by every renderer.
- Retry notice: `retrying in 3s (2/5): {error}`.

## Acceptance Criteria

- A render test sees the retry counter during a provider retry.
- `grep "app.status ="` finds only writes that a renderer reads.

Size: M. Source: UX sweep 2026-09-03 (frame).

## Outcome (2026-09-04)

Shown. The status line renders `app.status` in the activity slot
whenever it says more than the activity's own word — `retrying
provider (2/5)`, `running 3 tools`, `waiting for your answer`,
`compacting session` — capped at 36 columns so the model and meter
keep theirs; the bare word remains the fallback. Every write already
sat beside its activity change, so no site needed reordering. The
one hint that was never a status (`Ctrl-X: M models · T themes`) is a
notice only. The retry notice reads `retrying in 3s (2/5): …`. A
render test pins the retry counter and the running tool. The
vocabulary item (one label per `Activity` across the status line and
the activity row) is left for [Plain words](plain-words-on-every-surface.md).
