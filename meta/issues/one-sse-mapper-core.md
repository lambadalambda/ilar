# One SSE mapper core

## Summary

The three provider SSE mappers (openai.rs, zai.rs Anthropic + OpenAI
flavors) hand-roll the same five behaviors in parallel: the
event-after-terminal guard, stop-reason/tool-call consistency
validation, max-tokens null-input `ToolCallCompleted` synthesis, the
"args must parse to a JSON object" rule, and the identical
"stream ended before completion → RetryableError" `finish()`. A
contract fix must currently be re-derived per mapper (the
refusal/tool-call rule exists only in openai.rs). `wire_usage` and
zai's `merge_usage` also normalize the same usage wire shapes and
already disagree subtly; `required_str`/`required_zai_str` are
character-identical.

## Requirements

- A shared tool-call ledger + terminal-state helper in the provider
  layer that all three mappers use for the five behaviors.
- One usage-normalization function; one `required_str`.

## Acceptance Criteria

- Existing provider tests pass; the refusal/tool-call rule holds in
  all three mappers (new test on the zai flavors).

## Milestone

12 — Health sweep
