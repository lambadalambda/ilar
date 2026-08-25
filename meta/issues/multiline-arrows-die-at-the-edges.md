# Multiline arrows die at the edges

## Summary

`KeyCode::Up if input.is_multiline()` calls `input.move_vertical(-1)`
and discards its `bool` (input.rs:446-452), always answering
`Edited`. `move_vertical` returns `false` exactly when the cursor is
already on the first/last line — so in a multiline draft, Up on the
top line and Down on the bottom line are dead keys instead of
falling through as `Unhandled` (transcript scrolling), contradicting
the F1 help text. A buffer containing only `"\n"` is also both
`is_blank()` and `is_multiline()`, making blank-prompt history
recall unreachable there.

## Requirements

- Honor `move_vertical`'s return: `false` falls through as
  `Unhandled`.

## Acceptance Criteria

- A test: Up on the first line of a multiline draft is `Unhandled`;
  Up within the draft moves the cursor and is `Edited`.

## Milestone

12 — Health sweep
