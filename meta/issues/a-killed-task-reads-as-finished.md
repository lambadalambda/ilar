# A killed task reads as finished

## Summary

The `tasks` listing has two words for how a task stands
(subagent.rs:3520-3525): `running` when a live handle is in the
registry, `finished` when there is not. So a task that was cancelled,
one that died with the turn that started it, and one that crashed all
read as *finished* — the word for a task that did its work and has an
answer. The `last:` line then shows its final assistant text, which for
a killed task is a mid-flight thought and reads like partial findings.

Seen whole in a charachat session on 2026-09-16: Esc aborted the parent
turn at 16:30:36.612 and the detached review child died in the same
millisecond, mid tool call — which is the documented rule
(docs/interface.md, "the detached tasks *that turn* started"). Its log
simply stops: no result, no turn error, nothing saying it was killed.
At 16:53 the listing told the parent `finished · 22m ago` with a
plausible-looking `last:`, so the parent tried to resume it for the
findings — and the resume failed on an unrelated 404. The cancellation
notice for that same task arrived at 17:09, thirty-nine minutes after
the fact, because a held notification waits for the user's next
message.

A model reading that listing has no way to tell "it answered" from "it
was killed before it could".

## Requirements

- The listing distinguishes an ending that produced an answer from one
  that did not: cancelled, failed and finished are three words, not
  one.
- A task killed with its parent's turn records that ending in its own
  log, so a reader of the transcript is not left with a severed chain.
- A killed task's `last:` line is not presented as though it were the
  task's answer.

## Notes

The vocabulary half is the archived "Agent endings use one vocabulary"
issue's rule, applied to the one surface that did not get it.

Found while reading a real session, 2026-09-16.

Size: S. Source: session forensics 2026-09-17.
