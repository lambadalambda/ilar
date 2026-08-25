# Tolerate arguments before the tool name

## Summary

A live GLM-4.6V stream (terranigma session, child 271f0b12) emitted
an OpenAI-compatible `tool_calls` chunk carrying only
`{"index":0,"function":{"arguments":"{}"}}` — no id, no name, and no
prior chunk had started index 0. The zai OpenAI mapper hard-errors
the turn ("arguments arrived before tool start"). Probes show 4.6v
usually sends complete single chunks, so this ordering is rare but
real.

## Requirements

- The staging map buffers argument fragments for an index whose
  id/name have not arrived yet; when the name lands later, the call
  starts with the buffered arguments.
- A stream that finishes with buffered arguments but no name is
  still an error (genuinely malformed), naming the index.

## Acceptance Criteria

- Mapper tests: arguments-then-name ordering produces the same
  events as name-then-arguments; arguments-with-no-name-ever still
  errors at finish.

## Milestone

12 — Health sweep

## Outcome

The staging map buffers pre-name fragments (`StagedCall` with
early-fragment lengths) and replays them as identical deltas when
the name lands — the event sequence matches name-first ordering
exactly. A complete stream that never names the index is a hard
error from `map`; EOF with buffered fragments retries (a cut
connection is timing, not malformedness). The captured GLM-4.6V
chunk round-trips in a test. (2e801ac)
