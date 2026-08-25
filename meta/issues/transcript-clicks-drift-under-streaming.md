# Transcript clicks drift under a streaming turn

## Summary

Clicking a tool row (or "more") while a turn streams often does
nothing, and only starts working once the turn ends. The click target
is resolved at mouse-*up* time against the hit map of the *newest*
frame (`app.rs`), but with follow-tail on the transcript scrolls
between the frame the user aimed at and the frame the lookup uses —
the row that was under the cursor has moved up, and the lookup usually
lands on an untargeted row.

## Requirements

- Resolve the hit target at mouse-*down* dispatch time and store it;
  a click (up without drag) toggles the stored target, not whatever
  sits under the cursor by then.
- Pin the viewport while the mouse button is held: suspend follow-tail
  on mouse-down and restore it after the release, so neither a click
  nor a drag-selection races the stream.
- The pressed state cannot leak: clearing the selection (scroll
  commands, a modal stealing the release) restores follow-tail.

## Acceptance Criteria

- A test where rows shift between mouse-down and mouse-up still
  toggles the row that was pressed.
- A test that follow-tail is off while the button is held and back on
  after the release.
- Manually: unfolding a tool row mid-stream works on the first click.

## Milestone

11 — Beyond the terminal

## Outcome

The hit target is now resolved at mouse-down dispatch time and stored
(`transcript_pressed_target`); the release toggles the stored target
instead of re-reading the newest frame's hit map. While the button is
held, `update_scroll_metrics` stops following the tail (gated on
`selecting_transcript`, so no separate pin state can leak — any path
that clears the selection releases the pin, and `follow_tail` itself
is never mutated). Remaining drift window: the ≤50 ms between the last
rendered frame and the mouse-down dispatch, which terminal mouse
reporting cannot close.

Both failure modes are pinned by tests: a click whose row shifts away
between press and release still toggles the pressed row, and the
viewport holds still under a held button while the stream grows, then
resumes following on release.
