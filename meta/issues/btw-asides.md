# /btw: ask an aside that leaves no trace

## Summary

Mid-session you want a quick answer *about* the work — "which port was
it again?", "did we already rule out X?" — without that exchange
steering the model or polluting the history. Claude Code's `/btw` is
the model: question in, answer in a modal, session untouched.

## Requirements

- `/btw <question>` answers over the session's live transcript with
  the session's own model, system prompt and tools.
- Neither the question nor the answer is written to the session log.
- The request reuses the turn's own prefix (transcript untouched,
  question last, same cache key) so the conversation is served from
  prompt cache and the aside pays for the question alone.
- The answer displays in a scrollable modal; dismissing it is the end
  of the exchange.
- Mid-turn, the command waits like the other maintenance commands; a
  message queued during the aside still gets its turn afterwards.

## Acceptance Criteria

- Tests pin the request shape, the no-persistence guarantee, tool-use
  refusal, and cancellation.
- Tests pin the modal (question, answer, scroll clamp) and the queue
  release on completion.
- The full suite passes.

## Outcome

Landed in two commits: `ilar::aside::ask` (6091edb) and the TUI flow
(8b401d3). The core call is read-only — no writer lease, nothing
appended, which the tests pin by counting the session's events across
the call. The TUI rides the compaction plumbing (busy, cancellable,
idle-queued), and the queue decision is taken *before* the answer
modal opens, because a modal blocks the synthetic submit and nothing
would remain to release a message queued during the aside.

Filed retrospectively — the work was done before the issue, against
the usual order.

## Notes

- The aside instruction forbids tool calls but the tools stay in the
  request for cache identity; a model that calls one anyway is a loud
  error, same policy as compaction.
- Smoke-tested live: a session taught the word "pineapple" answered
  the aside with it, and the question appears in no session file.

## Milestone

11 — Beyond the terminal
