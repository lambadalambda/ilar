# The pending manager cannot scroll

## Summary

`render_pending_manager` (modals.rs:647) renders rows with
`.take(inner.height)` from index 0 — no window around the
selection — while navigation wraps over the full item list
(app.rs:1286-1288). The modal is fixed at height 14 (~12 rows); with
more pending items the selection marker scrolls off-screen and the
user can arm deletes on rows they cannot see. Every other list in
the file uses `list_window`. Bonus: the navigation hand-rolls
`ListNav::move_by`'s `rem_euclid` wrap instead of embedding
`ListNav` like every sibling modal.

## Requirements

- Window the rendered rows around the selection (`list_window`), and
  use `ListNav` for the selection state.

## Acceptance Criteria

- A test with 20 pending rows: the selected row is always within the
  rendered window.

## Milestone

12 — Health sweep
