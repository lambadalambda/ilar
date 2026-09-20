# A late arrival takes the resume offer

## Summary

`ends_mid_turn` reads a trailing `UserMessage` as "the session moved on
and nothing of that turn is left to continue", and returns false. That
is the only thing that sets `retry_available` on reopen, so it is the
only thing that offers Ctrl-R.

A task result that lands *after* an interrupted turn — delivered, or
salvaged, both of which append a user message without starting a turn —
therefore takes the offer away. The turn is every bit as resumable as
it was a moment earlier. Live the offer still shows, because that comes
from in-memory `turn_committed`; it vanishes when the session is
reopened.

## Requirements

- An arrival that starts no turn does not decide whether the previous
  turn ended mid-flight.

## Acceptance Criteria

- A test builds a log that ends mid-turn, appends a task-notification
  user message, and still gets the resume offer.
- A genuine user message after an interrupted turn still withdraws it
  — that is the rule this must not break.

## Notes

- Found reviewing the salvage-persistence change, 2026-09-20.
- Size: S. A notification envelope is recognisable
  (`task_notification_display` already does it), so the scan can tell
  the two kinds of user message apart.
