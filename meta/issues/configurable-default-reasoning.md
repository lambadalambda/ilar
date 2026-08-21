# Configurable default reasoning level

## Summary

Allow users to select the default reasoning/thinking variant for new sessions in layered TOML configuration.

## Requirements

- Add an optional `general.reasoning` setting.
- Apply normal project-over-user field precedence.
- Validate a configured reasoning variant against the selected model.
- Use the configured variant for new sessions while preserving resumed sessions' persisted variant.
- Document the setting in the example config and README.

## Acceptance Criteria

- Config tests cover the unset default and layered overrides.
- Startup-selection tests cover configured, resumed, and incompatible reasoning variants.
- A new session sends the configured reasoning options to its provider requests.
- Relevant tests, formatting, and clippy pass.
