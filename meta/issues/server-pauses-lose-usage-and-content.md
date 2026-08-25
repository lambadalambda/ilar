# Server pauses lose usage and content

## Summary

Three related defects in the `StopReason::Paused` continuation path
of `run_turn_inner` (turn.rs):

1. The Paused branch `continue`s without accumulating `acc.usage` or
   publishing `StepComplete` (turn.rs:1568-1590); the persisted
   message carries only the final continuation's usage, and a cancel
   with pending `paused_content` persists `Usage::default()`
   (turn.rs:1153). Paused segments bill real tokens that never reach
   the log or the UI counters.
2. `pause_retries` is initialized once per turn (turn.rs:1122) and
   never reset when a continuation chain settles (the step tail
   clears `continuations`/`continuation_provider`/`paused_content`
   only) — four unrelated pauses across a long agentic turn kill the
   whole turn with "pause retry limit reached".
3. The pause-path `anyhow::bail!`s (retry limit, changed provider,
   omitted replay content) skip the persist-partials dance the error
   path was built for — streamed text the user watched vanishes from
   the session log.

## Requirements

- Accumulate usage across pause segments (and persist it on cancel).
- Reset the retry counter when a continuation chain completes.
- Bail paths persist `paused_content` + accumulated blocks with a
  diagnostic, like the provider-error path.

## Acceptance Criteria

- Tests covering each: summed usage on a paused-then-completed step;
  a turn surviving pauses on separate steps beyond the per-chain
  limit; a bail leaving the streamed prefix in the session log.

## Milestone

12 — Health sweep
