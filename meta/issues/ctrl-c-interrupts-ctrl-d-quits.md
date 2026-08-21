# Ctrl-C interrupts, Ctrl-D quits

## Summary

Ctrl-C currently quits the session outright, from anywhere, with no
confirmation: the dispatcher intercepts it ahead of every modal, cancels
the turn, shuts the spawner down and returns `AppExit::Quit`. That is the
opposite of what the key means everywhere else — in a shell, in an
editor, in every other agent client, Ctrl-C is *interrupt*, and a running
turn plus a half-typed prompt are exactly what a reflexive Ctrl-C is
aimed at. One misfire ends the session.

The exit belongs on Ctrl-D, the EOF key: on a blank prompt, with no
overlay open, it quits.

## Requirements

- Ctrl-C never quits. It means what Esc means in whatever scope is open:
  dismiss the overlay, else abort the running turn, else clear the input.
- With nothing to interrupt (no overlay, idle, blank prompt) Ctrl-C says
  where the exit is instead of doing nothing silently.
- Ctrl-D on a blank prompt with no overlay quits — cancelling the running
  turn and shutting background jobs down the way Ctrl-C used to.
- Ctrl-D keeps its other two meanings, which the quit must not shadow:
  delete-forward while the prompt has text, and the session picker's
  delete confirmation while a modal is open.
- The half-page scroll pair that lived on Ctrl-U / Ctrl-D moves to
  Alt-U / Alt-D, so the exit key is unambiguous.
- Help overlay and README reflect the new bindings.

## Acceptance Criteria

- The scope decisions are pure functions under test: which scope a
  Ctrl-C interrupts, and the three conditions Ctrl-D needs to quit.
- Ctrl-C reaches the existing Esc paths rather than growing a second set
  of close/abort branches.
- Ctrl-D with text in the prompt still deletes forward; Ctrl-D inside the
  session picker still arms and confirms a delete.
- The full suite passes.

## Notes

- The dispatch half of the loop sits outside the `schedule` seam by
  design (see [A harness for the event loop's schedule](loop-schedule-harness.md)),
  so the wiring itself is covered by reading, not by a test; the
  decisions it consults are the part that gets tests.

## Milestone

7 — Unscheduled
