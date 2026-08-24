# The compaction cut measures tokens with a prose ruler

## Summary

`recent_steps_cut` sizes its recency window with `event_tokens` —
`chars/4 + 2` per event — an English-prose heuristic. Applied to
hexdumps, disassembly or base64 it underestimates by nearly 2×, so the
window silently doubles and compaction reclaims about half what it
intended. Measured in session `efd44d8a`: 1.06× error on one
compaction, 1.81× on the next, same code, same constants.

The information to fix it is already in the log. Every assistant
message carries the provider's reported prompt tokens for content we
can also estimate, so the error is measurable per session and the
estimate can be scaled by it. `estimate_tokens_from` even computes
both halves already — and then takes `max()`, throwing the ratio away.

## Requirements

- The cut scales its token estimate by the observed
  reported-versus-estimated ratio, clamped to a sane range and
  recomputed as the session runs.

## Acceptance Criteria

- A test replays this session's numbers: an 81k estimate against a
  148k reported prompt yields a cut near the intended budget.
- The full suite passes.

## Notes

- Deliberately not a tokenizer dependency (heavyweight, and wrong for
  every non-OpenAI model) and not content sniffing for hex or base64
  (fragile, and it only papers over the same gap).
- **Superseded, pending measurement, by
  [compaction as handover](compaction-as-handover.md).** With no
  token-budgeted window there is no estimate in the cut path at all;
  the trigger already uses the provider's reported count, which is
  ground truth. Reassess once the handover mode has run on real work.

## Milestone

11 — Beyond the terminal
