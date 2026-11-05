# A model picks what recall surfaces

## Summary

Replace BM25 ranking in `RecallConfig::recall` with a selector call:
one line per note (kind, age, title, summary — never the body), a prompt
adapted from Claude Code's (match on what the message is about, be
careful with preference and event notes, an empty answer is fine), at
most 5 picks.

## Requirements

- Candidates are numbered; the model answers with numbers, so it cannot
  invent an id. Unknown numbers are dropped.
- Model from config, default `openai/gpt-6-luna` (through the ChatGPT
  login), at its lowest effort. A failed or slow call costs the prompt
  its recall, never the turn. No BM25 fallback.
- The call starts with the turn; the first request waits a short grace
  for it, and a late answer joins at the next step.
- Notes the context already holds are not offered (existing
  `held_ids`); the per-window byte cap stays.
- Still off by default until the measurement says otherwise.

## Acceptance Criteria

- Pure tests: listing format, prompt, parsing (numbers, junk, out of
  range, more than 5), held notes left out.
- A turn test with a mock provider: a pick becomes a `MemoryRecall`
  event; a failing selector leaves the turn intact.
