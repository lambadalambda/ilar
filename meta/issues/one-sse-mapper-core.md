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

## Outcome

`provider/mapper.rs`: `MapperCore` (terminal flag + tool-call
ledger + per-flavor labels) now backs all three mappers for the
five shared behaviors; the refusal/tool-call rule holds on the zai
flavors too (deliberate fix, tested). One `merge_usage` reconciled
to the union of fields (zai gains cache_write_tokens); one
`required_str`. Wire formats untouched. (eb0f4ef)
