# Steer the agent you are looking at

## Summary

Once a focused agent fills the screen
([[a-clicked-agent-takes-the-screen]]), the obvious next want is to
talk to it. The safe path exists: a child's turn holds its
session's writer lock, so the way in is the steer machinery
(`message_task` / the steer queue), never a second writer.

## Requirements

- With a focus view open, typing routes to the focused agent as a
  steer (or a queued message when it is between turns), through the
  same durability guarantees steers earned (queue-first, survive a
  declined turn).
- The input line says who it is talking to; Esc still leaves.
- The root's own queued input is untouched by a focus excursion.

## Acceptance Criteria

- A steer typed in focus arrives in the child's transcript (visible
  in the focus view) and in the child's session log.
- A message to a finished agent resumes it or explains why not —
  the `task_message` semantics, from a keyboard.
- Root input stash/queue round-trips a focus session unchanged.

## Notes

- Parked until the read-only view has proven itself; scope
  deliberately excludes multi-writer anything.
