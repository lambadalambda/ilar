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

## Outcome

Two halves: 58d9e78 honors `move_vertical`'s return so edge arrows
are `Unhandled` rather than phantom edits; 3c28c6c adds
`InputBuffer::can_move_vertical` (factored from the same
edge-detection as `move_vertical`, so they cannot disagree) and
relaxes `handle_prompt_navigation_key`'s scroll guards from
`!is_multiline()` to "no row to move to" — edge arrows in a
multiline draft now scroll the transcript, mid-draft arrows still
move the cursor, blank-prompt history recall unchanged. The same
dead-key pattern found in the questions modal is recorded in
sweep-cleanups.

## Acceptance Criteria

- A test: Up on the first line of a multiline draft is `Unhandled`;
  Up within the draft moves the cursor and is `Edited`.

## Milestone

12 — Health sweep
