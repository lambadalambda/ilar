# Sessions know what they are about

## Summary

A session's title is the first 80 characters of its first message.
That is fine when the first message is "fix the flaky auth test" and
useless when it is a pasted stack trace, a PR link, or "hey can you
look at something". The session picker is a list of openings, not a
list of topics.

Give each session a short generated topic, shown where a session is
identified: the picker, and the running session's own header.

## Requirements

- After a session's first turn, a short topic (a handful of words) is
  generated from what was actually asked and answered, and persisted
  as a session event so it survives restarts and is visible to any
  reader of the log.
- Generation never blocks a turn and never fails one: it runs after
  the turn, and a failure leaves the session titled the way it is
  today.
- The picker prefers a generated topic and falls back to the first
  message, so old sessions keep working.
- The topic is bounded and sanitized: one line, no quotes or trailing
  punctuation, capped in width. A model that answers the conversation
  instead of naming it is rejected rather than stored.
- Generated once per session. Re-titling a long-running session is a
  separate question.

## Acceptance Criteria

- A test drives topic generation against a mock provider and asserts
  the event is appended and the picker prefers it.
- Tests pin the sanitizer: quotes stripped, trailing period dropped,
  newlines collapsed, over-long output truncated, empty or refusal
  output rejected.
- A test pins that a failed topic call leaves the session unchanged.
- The full suite passes.

## Notes

- Cost: one short call per session, on the session's own model. Not
  worth a separate cheap-model setting until it shows up in a bill.

## Milestone

10 — Everyday polish
