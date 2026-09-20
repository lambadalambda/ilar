# The focus view earns its chrome

## Summary

`render_focus` draws no activity row, no scrollbar, no tail/percent
title fragment — a stalled child and a scrolled-up view look
identical on the surface built for watching live agents. And the
slash-completion popup is gated only on modals, so a leftover `/…`
input pops "Tab/↵ complete" over a focus view whose keys route
elsewhere (view.rs:1013-1018).

Size: S-M. Source: sweep 2026-08-29, rendering.

## Outcome (2026-09-20)

All four. The title carries the same `· tail` / `· N%` fragment the
main transcript has always had; a running agent gets the activity row,
from the same `activity_line` the root ends with; and a timeline
taller than the view gets the scrollbar, on the same terms — none when
there is nothing to scroll.

The slash-completion leak was already fixed: `view.rs` gates the popup
on `self.focus.is_none()` as well as on modals.

Two things the work turned up that the issue did not name:

- **The title had to move.** It reports where in the timeline the view
  is, which the row count settles, and the row count comes from the
  cache — which was updated *after* the frame was drawn. So the first
  frame said nothing and every frame after said where the view had
  been. The rows are laid out against the width the border will leave
  before the border is drawn.
- **The activity row needed a line, not a line past the end.** It was
  appended after the visible rows had been trimmed to the viewport, so
  it was clipped and the view still said nothing. A running agent's
  viewport is one row shorter, which `max_scroll` then accounts for.
