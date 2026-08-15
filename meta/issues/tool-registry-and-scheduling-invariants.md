# Tool registry uniqueness

## Summary

The registry accepts duplicate names, causing duplicate provider definitions and ambiguous runtime lookup.

## Requirements

- Enforce unique tool names when composing registries.
- Register webfetch exactly once.
- Ensure provider definitions and runtime lookup use the same unique tool.

## Acceptance Criteria

- Duplicate registration returns a construction error.
- Full TUI registry definitions contain unique names.
