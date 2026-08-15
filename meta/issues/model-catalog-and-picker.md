# Model catalog and picker

## Summary

Model choices and context limits are hard-coded, incomplete, and selected by
cycling rather than through a discoverable menu.

## Requirements

- Maintain provider/model metadata from a documented models.dev source.
- Expose supported configured models rather than a small hard-coded shortlist.
- Use model-specific context-window limits in telemetry and compaction.
- Replace Ctrl-M cycling with a searchable keyboard model picker inspired by
  OpenCode's menu interaction.
- Persist selection before adopting the new model.
- Keep the picker usable on narrow terminals and dismissible without changes.

## Acceptance Criteria

- Supported OpenAI and z.ai models appear in the picker when their provider is
  configured.
- Search filters by provider, model ID, and display name.
- The active model and context limit update after confirmed persistence.
- Escape closes the picker and leaves the model unchanged.
- Picker rendering and navigation have focused tests.

## Notes

- Catalog source: https://models.dev/api.json, snapshot updated 2026-08-15.
- ChatGPT OAuth model access is narrower than OpenAI API-key access and follows
  the Codex backend slugs verified during OAuth integration.
