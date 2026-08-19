# Cumulative session usage + cost display

## Summary

Token usage is fully tracked but only the last turn is shown, and no
pricing exists despite the model catalog being snapshotted from models.dev,
which carries pricing data.

## Requirements

- Accumulate per-session totals (input, output, cache read/write tokens)
  across turns, surviving resume (recompute from session JSONL or persist).
- Add per-million-token pricing to the model catalog entries (input,
  output, cache read/write where the provider reports them).
- Display cumulative tokens and estimated dollar cost: compact form in the
  status line, full breakdown via a command palette entry.
- Unknown pricing (custom base_url models) degrades to tokens-only.

## Acceptance Criteria

- Unit tests for accumulation (incl. resume) and cost arithmetic.
- Status line shows a session total; palette shows the breakdown.
- Models without pricing never display a dollar figure.

## Notes

- Prices change; keep them in the same snapshot-dated catalog table.
