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

## Outcome (2026-09-20)

All of it, and one bullet turned out to be already done.

- **Model rows** carry the price per million (or `plan` where the
  model is billed that way), a `👁` for vision, and a `▸` where Enter
  opens the reasoning picker. Every one of those facts was already in
  `ilar::model`; the row simply never asked. The header names the
  level too: a picker opened to change the level could not say which
  one it was changing from.
- **The skill picker filters**, with the same `fuzzy_filter` the link
  picker uses, over the name and the description. It was the one list
  where typed characters fell on the floor, so it also had to become a
  paste target — the decider's two lists of modals now put it with the
  ones that have a query rather than the ones that discard.
- **Service rows** let the name give way and keep the detail, as an
  agent row does. Truncating `{name} · {detail}` from the right pushed
  `up 3m` and the exit reason off the line, where the docs promise
  "who died how".
- **The search preview footer** says `match 1 of N` rather than
  `event 37`, which was a number about our log file, not about the
  session anyone was reading. The index is still what the navigation
  uses; it is just not shown.
- **The todo panel's `+N hidden · ^T`** is clickable now by way of
  [[one-hit-map-for-the-sidebar]], which made a fourth clickable row a
  variant rather than a fourth copy of the plumbing.
- **`agents (N)` counting delivering rows** was already fixed: the
  title renders `agents (1) · 2 jobs · 1 delivering`.
