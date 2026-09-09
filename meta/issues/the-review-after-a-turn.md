# The review after a turn

## Summary

Hermes forks the conversation after every turn on the warm prompt
cache and asks whether anything durable happened — a repeated
correction, a workflow that worked, an error recovered from — and
writes a memory entry or a skill, biased toward action. ilar knows
exactly when the cache is about to go cold (`cache_compact`'s timer),
so the review can run once per idle episode rather than per turn,
which on 2026-09-05..07's numbers is the difference between a cheap
addition and doubling the request count.

## Requirements

- Once per idle episode, just before the cache window closes, and
  only if the episode crossed a threshold (Hermes's: five or more
  tool calls, an error recovered from, or an explicit correction).
- The turn's own request with a fixed instruction appended, like
  compaction: served from the cache. The instruction may add memory
  entries, file notes, or patch a skill, and must say "nothing"
  freely; no action bias.
- A one-line notice in the chat when it wrote something.
- `gateway.review.approval`: writes are staged under `<home>/pending/`
  and applied with `/pending` and `/approve`.

## Acceptance Criteria

- Tests: below the threshold nothing runs; above it one review runs
  per episode and its writes land (or stage); the notice is sent.
