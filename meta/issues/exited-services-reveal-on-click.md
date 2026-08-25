# Exited services reveal on click

## Summary

The services panel now hides exited services behind a muted "N
exited" count — but the only way to see *which* service died and how
is scrolling back to the transcript. The count line should be a
disclosure: click to list the exited rows with their exit details,
click again to fold them away.

## Requirements

- The "N exited" line carries a disclosure marker (▸/▾) and toggles
  on click; expanded, the exited services list under it with their
  details, muted.
- The sidebar gets its first hit-rect: the toggle line's screen
  position is recorded at render time and checked on mouse-down
  before the transcript takes the click.
- Hovering the toggle underlines it, like every other clickable.

## Acceptance Criteria

- A render-and-click test: the click flips the disclosure and the
  exited names appear; a second click folds them.
- The hit rect clears when the panel is not drawn.

## Milestone

11 — Beyond the terminal

## Outcome

The "N exited" line is a ▸/▾ disclosure: its screen rect is recorded
at render time (cleared every frame so a hidden panel cannot take
phantom clicks), mouse-down checks it before the transcript, hovering
underlines it via the raw pointer position, and clicking lists the
exited services with their exit details in muted rows — click again
to fold. Pinned by a render-and-click test covering reveal, fold,
miss, and the cleared rect.
