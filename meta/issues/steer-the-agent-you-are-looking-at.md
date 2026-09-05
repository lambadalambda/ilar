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

## Outcome (2026-09-05)

With a focus view open the prompt is the agent's: keys go to the input
(the scroll keys and Esc keep their view meaning), the input title
reads `to explorer · survey the API`, and Enter sends the text through
`SubagentSpawner::message_task` as a detached task — a running agent is
steered, a finished one resumed with the message as its prompt. The
root records the send as `→ <agent>: <text>` and, when the task ends,
the agent's answer or the failure as a transcript line; the root's own
queue and stash are untouched, and quitting aborts messages in flight.
Help lists the key. Tested: the title and the listening border under a
focus view, the transcript line's shape. Not live-tested with a real
child; the path is the model's own `task_message`, which is.
