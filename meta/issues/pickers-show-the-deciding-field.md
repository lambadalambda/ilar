# Pickers show the deciding field

## Summary

- Model picker (modals.rs:3109-3126): name, id, context — no price or
  plan, no vision flag, no hint that Enter opens a second (reasoning)
  picker for models with levels; the `current {model}` header (3081)
  omits the active level.
- Skill picker (1169-1196) has no filter; `/` completion already does.
- Todo panel note `+N hidden · ^T` (sidebar.rs:410-415) is not
  clickable while the neighbouring `+N more` and `N exited` are.
- Service rows right-truncate `{name} · {detail}` so a long name pushes
  `up 3m` and the exit reason off (sidebar.rs:234-238) — docs promise
  "who died how".
- `agents (N)` counts delivering rows as agents (view.rs:882).
- Search preview footer shows `event 37` (modals.rs:2205), an
  internal index.

## Requirements

- Model rows: price or `plan`, `👁`/`vision` flag, `▸ levels` suffix,
  current level in the header. Skill picker: `fuzzy_filter` like the
  link picker. The rest as stated.

Size: M. Source: UX sweep 2026-09-03 (overlays).
