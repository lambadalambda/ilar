# Replay message item ids

## Summary

Codex and OpenCode send the `msg_…` id back on replayed assistant
message items; ilar sends none (it keeps only `function_call` ids). The
effect is unknown, possibly on cache hits.

## Requirements

- Measure first: cache-read tokens over a replayed session with and
  without message ids, on the ChatGPT backend and on Zen.

## Acceptance Criteria

- A measurement in this file; code only if it shows a difference.
