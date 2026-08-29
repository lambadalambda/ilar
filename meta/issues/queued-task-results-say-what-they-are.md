# Queued task results say what they are

## Summary

Two honesty gaps found in review, both about notification texts
that left the notification machinery and became ordinary messages:

- A steered-then-unread task result is spliced into the message
  queue at turn end. The quit warning counts held and in-flight
  results but not these, so Ctrl-D can stay silent while a result
  waits in the queue — durable in the outbox, so nothing is lost,
  but the warning's count lies low.
- The pending manager shows that queue entry as a plain message:
  the user can edit or delete a `<task-notification>` without
  knowing what it is. Deleting is recoverable (outbox redelivers
  at next open) but nothing says so.

## Requirements

- The undelivered count includes queued and pending-steer entries
  that are task/tool notification envelopes.
- The pending manager labels such entries as task results
  (reusing the collapsed-headline formatter) and says deletion
  only defers them to the next open.

## Milestone

13 — Guard rails
