# Anthropic flavor caps output at 16k

## Summary

The z.ai Anthropic flavor hardcodes `"max_tokens": 16_384`
(zai.rs:70) while the catalog says these models emit up to 131_072 —
long generations (including thinking, which counts against
`max_tokens` on GLM) truncate at 16k with `MaxTokens` and null-input
tool synthesis. The same magic number recurs as
`context_limit.saturating_sub(16_384)` in `Config::input_limit`
(toml.rs:543-553); `max_tokens` is absent from the reserved-options
list, so a user override silently desynchronizes the two.

## Requirements

- Derive `max_tokens` from the catalog's output limit (one source);
  `input_limit` uses the same value.
- Either reserve `max_tokens` in options or make an override flow
  into both places.

## Acceptance Criteria

- A wire test: the request's `max_tokens` matches the catalog's
  output limit for the model.

## Milestone

12 — Health sweep

## Outcome

`zai::max_output_tokens(model)` is the single source: the catalog's
`output_limit` (131k for the V/flagship models), 16_384 floor for
uncataloged ids. The Anthropic wire uses it and
`Config::input_limit` subtracts the same value; `max_tokens` is now
reserved in options so an override cannot desync the pair.
Compaction triggers at the identical token count as before (the old
`context − 16k` term never won the `min()`). The uncataloged-model
asymmetry in the config fallback path is recorded in
sweep-cleanups. (3289226)
