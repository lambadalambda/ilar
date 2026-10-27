# GPT-6.1 Terra

## Summary

OpenAI released `gpt-6.1-terra` on 2026-10-26. models.dev lists it
under `openai` only (not Zen or Go). Codex lists it for every ChatGPT
plan, with no access gate.

## Requirements

- One `openai` row (`OpenAiBoth`): 272k working window and input (the
  price doubles past 272k; Codex says 500k, models.dev 1,050,000
  context and 922,000 input), 128k output, vision,
  `EFFORT_NONE_TO_MAX` (models.dev's ladder; Codex starts at low, as it
  does for 5.6 Terra).
- Price, base tier: 1 / 5 / 0.05 / 1.25 (in, out, cache read, cache
  write).
- No `opencode` row until Zen lists it.

## Acceptance Criteria

- A catalog test covers the row, its ladder and its price.
- Full gate green.
