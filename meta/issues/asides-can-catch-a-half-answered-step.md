# Asides can catch a half-answered step

## Summary

`/btw`'s `settled()` (aside.rs:31-42) pops trailing *assistant*
messages with tool calls — but the live turn appends tool results
one at a time with an awaited publish between appends
(turn.rs:1861-1907), and `transcript_of` flushes partial
`pending_results` as a trailing user message. An aside snapshotted
in that window sees `[..., assistant(N calls), user(M<N results)]`,
pops nothing, and sends a request carrying unanswered `tool_use`
blocks — providers reject it with a 400.

## Requirements

- `settled()` also drops a trailing partial tool-result user message
  (and then the now-unanswered assistant message), so the snapshot
  always ends on a settled boundary.

## Acceptance Criteria

- A test: a transcript ending in a partial result set yields an
  aside request with no unanswered tool calls.

## Milestone

12 — Health sweep

## Outcome

`settled()` now loops on an `unsettled_tail` predicate that also
recognizes a trailing user message whose tool-result ids don't
cover the preceding assistant's call ids (matched by id, not
count) — popping the partial results, then the unanswered step,
until the tail settles. Fully-answered steps still ride along.
Pinned by three in-file tests. Known trade-off: a queued user text
merged into the same message as partial results is dropped from the
aside snapshot only (the log is untouched).
