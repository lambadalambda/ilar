# Model picker applies default reasoning selection

## Summary

Selecting gpt-6-astra with the default reasoning level in the model picker appears not to change the active model.

## Requirements

- Apply a selected model when its default reasoning level is chosen.
- Preserve explicit reasoning-level selections.

## Acceptance Criteria

- A regression test reproduces the default-selection failure and passes with the fix.
- Relevant model picker and model-switch tests pass.

## Resolution

The variant picker now receives the current model as well as its reasoning level.
Only an unchanged model/level pair dismisses without applying; a different model
with provider-default reasoning emits `Choose(None)`. Active-row marking follows
the same identity check.

Verified red → green for Astra default selection; coverage includes both OpenAI
and OpenCode, explicit reasoning, and unchanged selections. All 458 TUI tests,
`cargo fmt --all -- --check`, and
`cargo clippy -p ilar-tui --all-targets -- -D warnings` pass.
Independent review found no blockers.
