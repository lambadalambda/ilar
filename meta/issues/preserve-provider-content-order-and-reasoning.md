# Preserve provider content order and reasoning

## Summary

The agent reorders streamed content blocks and drops opaque OpenAI reasoning items required for stateless tool continuation.

## Requirements

- Preserve text, thinking, reasoning, and tool-call order exactly as streamed.
- Preserve multiple thinking runs and signatures independently.
- Request encrypted OpenAI reasoning content when `store:false`, persist opaque items in order, and replay them with function-call continuation.
- Open and close z.ai `reasoning_content` runs correctly.
- Downgrade incomplete or unsigned Anthropic thinking to non-replayed diagnostic text rather than serializing `signature:null`.

## Acceptance Criteria

- Tests cover text-tool-text order and multiple thinking blocks.
- A two-request OpenAI Responses tool round-trip includes prior reasoning items.
- Anthropic signature deltas concatenate and valid signatures round-trip.
- Tests cover thinking-text-tool-text and z.ai reasoning completion boundaries.

> Historical note (2026-08-26): thinking `signature` preservation
> described here was removed with the zai Anthropic flavor — see
> sweep-deferred-decisions.
