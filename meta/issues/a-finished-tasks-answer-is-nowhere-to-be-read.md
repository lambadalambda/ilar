# A finished task's answer is nowhere to be read

## Summary

A background task's result reaches its parent exactly once, as a
completion notification, and a notification can wait: an aborted turn
holds them until the user's next message, and a busy parent is not
interrupted. Meanwhile the `tasks` listing — the only window the model
has — shows a finished task as `finished · 7m ago` with its last
assistant text and *no result*, and says nothing about one being on its
way.

So a model that checks on a task it is waiting for is told the work is
done and handed nothing. In a charachat session on 2026-09-16 the
parent did the reasonable thing with that: at 17:04:36 it resumed the
finished task to ask it to resend its findings — a second full
subagent run — and at 17:09:36 both copies of the same review arrived
anyway, batched behind the user's next message.

The tool's own description tells the model a finished task "is resumed
from its transcript with your message as its prompt", so re-asking is
exactly what the documentation suggests. The listing is what should
have stopped it.

## Requirements

- A finished task's listing says whether its result has been delivered
  yet, and does not read as "nothing to report" when a notification is
  queued.
- The parent can read a finished task's result without spending a
  model call on it — the listing carries it, or names where it is.
- Resuming a task only to re-fetch a result it already produced stops
  being the cheapest path to it.

## Notes

The holding itself is deliberate (docs/interface.md; an abort must not
start a follow-up turn nobody asked for). What is missing is the
listing telling the truth about it while it waits.

Found while reading a real session, 2026-09-16.

Size: S-M. Source: session forensics 2026-09-17.
