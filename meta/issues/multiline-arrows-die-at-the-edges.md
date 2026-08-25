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
  `Unhandled`. (Done in 58d9e78.)
- Discovered while fixing: `Unhandled` does not actually reach
  transcript scrolling — `App::handle_prompt_navigation_key` runs
  *before* `handle_prompt_key` and guards its scroll arms with
  `!is_multiline()`, and main.rs's `Unhandled` arm is a no-op. For
  the F1 help promise ("Up / Down scroll"), the app.rs guard must
  admit edge-of-draft arrows (e.g. ask the input whether a vertical
  move can succeed).

## Acceptance Criteria

- A test: Up on the first line of a multiline draft is `Unhandled`;
  Up within the draft moves the cursor and is `Edited`.

## Milestone

12 — Health sweep
