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
