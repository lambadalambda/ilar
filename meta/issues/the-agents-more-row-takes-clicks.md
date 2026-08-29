# The agents "more" row takes clicks

## Summary

The agents panel in the top-right sidebar collapses past
`AGENT_PANEL_MAX` entries into a "+N more" row — and that row is
inert text. The services panel next to it already solved this
exact problem: its exited-services disclosure row carries a click
hit-rect, a hover underline, and a toggle. The agents panel got
the collapse without the disclosure, so the one row that promises
more information is the one row that gives none.

## Requirements

- Nothing is hidden unless space actually runs out: the fixed
  `AGENT_PANEL_MAX` content cap goes away, and the panel shows every
  agent that fits its height budget (the same half-of-sidebar cap
  `carve_panel` already enforces).
- Only under real space pressure does the tail collapse into a
  "+N more" row — and that row takes clicks: expanding lets the
  panel use the whole sidebar column, at the todo list's expense,
  which is the clicker's explicit choice.
- The expanded panel offers the way back (a "show less"-style row),
  clickable the same way, and collapses by itself when the pressure
  disappears.
- The row underlines on hover like every other clickable, and only
  when the mouse actually reaches content (`mouse_reaches_content`).

## Acceptance Criteria

- With more than `AGENT_PANEL_MAX` agents, a click on the "+N more"
  row shows all of them; a click on the collapse row returns to the
  bounded view.
- Hovering either row underlines it; hovering anything else in the
  panel does not.
- A panel at or below the limit renders no disclosure row and takes
  no clicks.

## Notes

- Mirror the services panel's `exited_toggle` plumbing: row index
  out of `agent_panel_lines`, screen rect via the same hit helper,
  dispatch where `services_exited_hit` is handled.
- Surfaced twice by the user; the second report ("still can't
  click") landed after the rows-react sweep, which covered
  transcript rows but never reached this sidebar row.
