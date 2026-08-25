# Restored nested thoughts collide

## Summary

The restore path assigns thought/note ids from each invocation's
*local* line count (`thought:restored:{lines.len()}`,
session_view.rs:231-236), and `restore_child_activity` runs the same
function for child sessions — so a child's `thought:restored:1`
collides with the parent's. The live path gives nested thoughts
empty ids on purpose ("not expandable"), and the click handler
(`toggle_transcript_target`) only scans top-level lines. Clicking a
restored nested thought toggles an unrelated top-level thought or
silently does nothing.

## Requirements

- Nested restored thoughts/notes get empty ids like the live path
  (or ids namespaced per session and a click handler that can reach
  them — pick one, consistently).

## Acceptance Criteria

- A test restoring a session with a child session: nested thought
  rows carry no clickable target, and clicking a top-level restored
  thought toggles only itself.

## Milestone

12 — Health sweep

## Outcome

Restored lines get ids from `restored_line_id(nested, ...)`, which
returns an empty id for any nested invocation
(`parent_tool_call_id.is_some()`) — matching the live path's
invariant that nested thoughts/notes are previews, not click
targets; recursion covers deeper levels for free. Top-level ids are
unchanged and unique. Pinned by
`restored_nested_thoughts_are_not_click_targets`, which also renders
the rows and asserts every emitted `Thought` hit target is
top-level.
