# GPT-6 Astra

## Summary

OpenAI released `gpt-6-astra` on 2026-09-04 (models.dev: 1,050,000
context, 922,000 input cap, 128,000 output, vision, effort ladder
low/medium/high/xhigh/max, $10/$50 with $1 cache read and $12.5 cache
write, doubling past 272k input). OpenCode Zen serves it on the
Responses wire; Go does not list it. ilar has no row, so it is
reachable only as an uncataloged `openai/gpt-6-astra` with the 128k
fallback window and no ladder.

## Requirements

- `openai/gpt-6-astra` and its `opencode/gpt-6-astra` twin, with the
  working window Codex uses if it declares one, models.dev's otherwise;
  the ladder verbatim (no `none` rung); vision; the base-tier price.
- `OpenAiBoth` only if the ChatGPT backend answers for it; `OpenAi`
  otherwise, so a ChatGPT-only config does not list a dark row.

## Acceptance Criteria

- Both rows in the catalog and priced; the twin test holds; a live
  one-token turn through each configured route answers.
