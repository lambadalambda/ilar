# Compaction correctness and cancellation

## Summary

Interactive turns bypass configured compaction, historical usage can retrigger it forever, and compaction ignores cancellation and terminal stream validity.

## Requirements

- Apply the same configured loop settings to every turn-launch path.
- Estimate only the active transcript after the latest compaction boundary.
- Persist a summary only after `TurnComplete(EndTurn)`; reject EOF, MaxTokens, Paused, Refusal, and partial output.
- Honor cancellation before and during compaction.
- Include system-prompt and tool-schema characters in first-turn estimates.

## Acceptance Criteria

- Enter-driven turns compact at the configured threshold.
- Old pre-compaction usage does not retrigger compaction.
- EOF without completion rejects a partial summary.
- Escape cancels an in-flight compaction promptly.
