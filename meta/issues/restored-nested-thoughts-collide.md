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
