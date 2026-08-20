# TUI module follow-ups

## Summary

Small items the module split surfaced but deliberately left alone,
because a move-only commit is only reviewable if it does nothing else.
Each is independent; none is urgent.

## Requirements

- **`render_pending_manager` should not take `&App`.** It is the last
  thing keeping `use crate::App` in `modals.rs`, and it reads seven
  members (`pending_manager`, `pending_items()`, `queued_messages`,
  `goal`, `background_running`, `services_running`, `last_prompt`).
  `sidebar.rs` already shows the shape: a `pending_snapshot(&App)` in
  `app.rs` plus a renderer over the snapshot. Removes the last
  `app ↔ modals` cycle and about seven `pub(crate)` markers.

- **The in-progress todo marker has two colours.** `sidebar.rs`
  `render_todo_sidebar_snapshot` paints it `theme::WAITING`;
  `todo_summary` paints the same state `theme::RUNNING`. Two renderings
  of one todo state disagree. Previously ~200 lines apart in main.rs, now
  adjacent, which is how it was noticed. Pick one.

- **`sidebar.rs` owns less than its name.** The goal panel, the services
  panel and the todo `Block` still render inside `App::render`; the
  module owns only the todo content lines. The panel-carving arithmetic
  is pure `Rect` geometry, the same shape as `content_areas`, which
  tested well.

- **`slash_candidates` sits in main.rs.** It is a pure function over the
  input text with no `App` dependency — `input.rs`'s concern, or a
  completion module if one appears.

- **`glob` patterns match nothing, silently.** The `glob` crate has no
  brace expansion, so `**/{route,client}/*.ts` returns "(no matches)"
  after paying for the walk. Models write brace patterns routinely.
  Either support them or reject them loudly. (Recorded during the glob
  work; unrelated to the TUI, kept here to avoid a single-line issue.)

## Acceptance Criteria

- Each item either done or explicitly declined with a reason.
- No behaviour change beyond the todo-colour decision, which is a
  deliberate one-way pick.

## Notes

- `scripts/split_module.py` and `scripts/minimize_visibility.py` exist
  for this kind of work and both now refuse rather than guess when their
  assumptions break — see their docstrings before reusing them.

## Milestone

6 — Hardening
