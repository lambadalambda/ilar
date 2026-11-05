# Know the partial_answer phase

## Summary

Codex's `MessagePhase` has three values: `commentary`, `partial_answer`,
`final_answer` (`protocol/src/models.rs`). ilar knows two and drops the
third as unknown, so such text replays with the derived phase instead
of the one the model sent.

## Requirements

- `TextPhase::PartialAnswer`, stored and replayed like the others.

## Acceptance Criteria

- Parser test: a message item with `phase: "partial_answer"` yields
  `TextPhase::PartialAnswer`; replay sends `"partial_answer"`.
