# Hover underlines clickable transcript rows

## Summary

Tool rows, groups and thought headers expand on click, but nothing
marks them as clickable — the affordance is invisible until tried.
With pointer position now tracked (motion events are coalesced
anyway), the row under the pointer can advertise itself: hovering a
row that has a click target underlines its text, the terminal
equivalent of a link hover.

## Requirements

- Hovering a transcript row with a hit target underlines that row's
  text spans; rows without a target are unchanged.
- Indent and box-drawing spans (branch glyphs, padding) stay bare —
  only content is underlined.
- The underline follows the pointer, not the content: it always marks
  what a click would hit right now.
- No effect while a modal other than transcript search is in front
  (clicks do not reach the transcript there either).

## Acceptance Criteria

- A test that a hovered targeted row gains underline on its content
  spans and an untargeted row does not.
- Manually: sweeping the pointer over a tool group shows the
  underline riding the hover.

## Milestone

11 — Beyond the terminal

## Outcome

The pointer position (viewport-relative, resolved through the same
`selection_point` the click path uses) lives in `App::hover`; at
render time the hovered row gets `underline_content_spans` if and only
if it carries a hit target and no modal other than transcript search
is in front. Whitespace and box-drawing spans stay bare, so indent and
branch glyphs do not grow underlines. Verified by a render-to-buffer
test plus a live tmux smoke test: synthesized SGR motion events moved
the underline across thought and tool-group rows, left plain text
bare, and a subsequent click expanded the hovered group.
