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

## Outcome

`render_pending_manager` windows rows with `list_window` around the
(clamped) selection like every sibling; the manager's bare
`selected` field became a private `ListNav` with
`select`/`move_selection` that disarm on move, and stale click
indexes clamp. Pinned by
`a_scrolled_pending_manager_keeps_the_selection_on_screen`,
`pending_manager_navigation_wraps_over_the_whole_list`, and
`a_stale_pending_click_lands_on_the_last_row`. The inert mouse
wheel is recorded in sweep-cleanups. (0d9737d)
