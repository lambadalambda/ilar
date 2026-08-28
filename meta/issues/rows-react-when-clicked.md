# Rows react when clicked

## Summary

Two reports — "expandable things don't get underlined on hover" and
"take a long time to unfold" — traced to four independent causes,
plus two places where the underline lies.

## Requirements

- **A click that jitters one cell is still a click.** Any drag
  event sets `transcript_dragged`, and the toggle requires none, so
  press → twitch → release copies text or does nothing. Tolerate a
  small movement (or an empty selection range) before deciding a
  press was a drag.
- **Expanding must not re-render the whole transcript.** All three
  toggle arms mark the cache dirty from index 0 (`lines_mut` calls
  `touch_whole_transcript`), so one click re-parses markdown,
  re-wraps and re-highlights every entry above it. Mark from the
  toggled index; nothing above it can change.
- **An expanded, still-running agent row must not deep-clone and
  re-render its child transcript every frame** (it is `animated`
  while the child runs, so this is up to 20 fps at exactly the
  moment the user is watching it). Same for a tool group holding
  one live call re-rendering its expanded siblings.
- **Detail rows wrap the whole 16 KiB payload and then keep 4-8
  rows.** Cut before wrapping.
- Cheap: `complete_open_thought` scans the whole transcript on
  every text delta; the subagent-activity retry is O(queue ×
  queue × transcript) and can wedge the loop if any activity can
  never attach; `slash_inventory()` is rebuilt every frame whether
  or not the input starts with `/`.
- **Nested subagent reasoning rows draw a `+` and cannot be
  expanded** (their ids are forced empty): give them ids or stop
  advertising.
- **Inline markdown links are permanently underlined and are not
  clickable** — the inverse lie, which makes hover meaningless in a
  link-heavy reply. Either make them click targets or stop
  underlining them.
- Lower: modal rows are clickable and never underline; a
  single-line Task/Job row underlines and toggles nothing; the
  services disclosure is clickable under the Search modal but only
  underlines when no modal is open.

## Acceptance Criteria

- Toggling a row in a long session marks only that row's index.
  A jittered click still expands. Tests where the shape allows.

## Milestone

13 — Guard rails
