# Todo sidebar narrow-terminal fallback

## Summary

The todo sidebar only renders at width ≥ 121 columns; narrower terminals
lose todo visibility entirely.

## Requirements

- Below the sidebar threshold, render a compact one-line todo strip above
  the input: current in-progress item + counts (e.g. `▸ fix parser (2/5)`).
- The strip appears only when the todo list is non-empty; zero-height cost
  otherwise.
- Sidebar behavior at ≥ 121 cols is unchanged.

## Acceptance Criteria

- Layout tests at narrow widths show the strip and no sidebar; wide widths
  show the sidebar and no strip.
- No panic at extreme sizes (e.g. 20×5).

## Notes

- Truncate the item title with an ellipsis to fit.

## Resolution

Already implemented before this issue was filed: below the sidebar
threshold, `todo_summary` renders a one-line strip (status marker,
current item, `+N` hidden count) as the transcript block's bottom border
title, covered by `narrow_todos_use_border_chrome_instead_of_transcript_rows`.
Closed with no code change.
