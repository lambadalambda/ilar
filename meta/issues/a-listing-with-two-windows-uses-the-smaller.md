# A listing with two windows uses the smaller

## Summary

Discovery read `context_length` and fell back to `max_context_window`.
llama.cpp reports `context_length` as the total across its parallel
slots — 512k with two slots of 256k — so a request believed it had
twice the window it does; Lemonade reports the reverse, a configured
window below the model's maximum. The smaller of the two is the one
a request can use.

## Requirements

- When a listing states both, the smaller wins; one alone is taken
  as is; the endpoint's `context` and the default follow as before.

## Acceptance Criteria

- Test: a row with `context_length` 524288 and `max_context_window`
  262144 resolves to 262144.

## Notes

- Done 2026-09-14.
