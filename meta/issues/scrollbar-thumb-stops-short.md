# Scrollbar thumb stops short of the bottom

## Summary

At the tail of the transcript the scrollbar thumb never reaches the
end of its track — it stops a few rows above it, and the gap grows
with the terminal height (four rows at 40 lines). The transcript is at
its true tail, so the bar says "there is more below" when there is
not.

## Requirements

- At maximum scroll the thumb is flush with the end of the track; at
  the top it is flush with the start.

## Acceptance Criteria

- A test pins both ends at several terminal heights.
- The full suite passes.

## Notes

- Cause: ratatui's `ScrollbarState::content_length` is one past the
  last scroll position, where that position puts the last line at the
  *top* of the viewport. Our scrolling stops with the last line at the
  bottom, so the row count overstates the range by a viewport.

## Outcome

Fixed by handing the scrollbar our position count (`max_scroll + 1`)
instead of the row count; thumb length is unchanged because it derives
from `viewport_content_length`. Verified live in tmux at 40 rows: top,
middle and tail all sit where they claim.

## Milestone

10 — Everyday polish
