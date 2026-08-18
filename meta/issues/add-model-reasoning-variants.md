# Add model reasoning variants

## Summary

Expose model-specific reasoning levels as selectable variants so users can tune
thinking effort without manually editing provider options.

## Requirements

- Define only the reasoning variants supported by each model/provider.
- Let model selection flow into variant selection when variants are available.
- Persist the selected variant with the session and apply it to subsequent turns.
- Show the active variant in the TUI without crowding narrow terminals.
- Preserve existing model selection for models without variants.

## Acceptance Criteria

- Supported models expose clear thinking-level choices.
- Selecting a variant changes the provider reasoning option on the next turn.
- Resumed sessions restore the selected variant.
- Unsupported provider/model combinations cannot receive invalid reasoning levels.
- TUI and workspace checks pass.
