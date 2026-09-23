# Every ending says its messages wait

## Summary

[[a-detached-resume-says-its-message-waits]] put a "still queued" line
on a detached task's notification. Two endings still go without it,
and in both a message can be waiting that the sender was told nothing
about:

- A nested hop. A result routed to a child runs a turn with no live
  channel, so a message its parent sends meanwhile is held for the
  next resume. The hop's report to that parent ("Nested task … completed")
  says nothing, and the parent reads the message as lost.
  The route's failure replacements ("its workspace could not be
  restored") have the same gap.
- The panic fallback. A task that dies without reporting sends "ended
  abnormally", and does not know which session it was, so it cannot
  count what waits.

## Requirements

- A propagated or replaced hop notification carries the queued line
  for the session the hop ran in, counted after that turn's hold has
  given back what it took.
- The abnormal-ending notification for a task carries it too; the
  guard learns the task's session and the steer store.

## Acceptance Criteria

- A test: a message sent while a routed turn runs appears as queued in
  the hop's notification to the parent.
- A unit test: a dropped reservation for a task with a waiting message
  says so.
- Full gate green.

## Notes

- Source: review of the queued note, 2026-09-23. Size: S.
