# GPT-6 Sol and Luna

## Summary

OpenAI released `gpt-6-sol` and `gpt-6-luna` on 2026-09-22. models.dev
lists both under `openai` and `opencode` (Zen), not `opencode-go`.
Codex lists both for every ChatGPT plan, without astra's access gate.
Four existing GPT prices also moved after `CATALOG_UPDATED`
(2026-08-15).

## Requirements

- Rows for both on `openai` (`OpenAiBoth`) and `opencode`
  (`OpenCodeResponses`): 272k working window and input (the 5.6 and
  astra rows' convention; models.dev says 1,050,000 context, 922,000
  input cap, and the price doubles past 272k), 128k output, vision,
  `EFFORT_NONE_TO_MAX` with reasoning summaries.
- Prices, base tier, both providers: Sol 2 / 10 / 0.2 / 2.5, Luna
  0.1 / 0.5 / 0.01 / 0.125 (in, out, cache read, cache write).
- Corrected: `openai/gpt-5.6-sol` and `openai/gpt-5.6` 4 / 20 / 0.4 / 5
  (price cut, models.dev 0b2318a6, 2026-08-25); `opencode/gpt-5.6-sol`
  4 / 20 / 0.4 / 5 (Zen discount ended, 156818aa, 2026-09-18);
  `opencode/gpt-5.6-terra` 2.5 / 15 / 0.25 / 3.125 (Zen's price since
  2026-07-10, copied from the `openai` twin by mistake).
- `CATALOG_UPDATED` moves to 2026-09-23.

## Acceptance Criteria

- The catalog tests cover the new rows' twins and ladders.
- Full gate green.
- A live one-token turn on each route, when a key is at hand (as for
  astra).

## Notes

- Not done here, for a decision: `CHATGPT_SUGGESTED_MODEL` could move
  from `gpt-5.6-sol` to `gpt-6-sol` (Codex now suggests that upgrade);
  models.dev marks `gpt-5.2-chat-latest` and `gpt-5.3-chat-latest`
  deprecated, and the catalog drops deprecated rows.
- Left out: Codex's `ultra` rung, and the experimental `fast`
  (priority tier) and `pro` modes — none is in the API docs.
- Source: user request, 2026-09-23; data from models.dev, OpenAI's
  model pages and Codex's `models.json`. Size: S.

## Done (2026-09-23)

Rows, prices and the four corrections in `5e9469b`. Live on Zen:
`ilar exec --model opencode/gpt-6-luna` and `…/gpt-6-sol` both
answered "ok" on tenco. Not probed: the `openai` rows over an API key
or a ChatGPT login — no key on the boxes, and the Mac's binary
predates the rows. The two open decisions in the notes stand.
