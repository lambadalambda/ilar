# Extract provider transport

## Summary

OpenAI and z.ai duplicate HTTP pump, panic handling, bounded error, and abort-on-drop mechanics.

## Requirements

- Extract only shared transport lifecycle behavior after protocol handling is stable.
- Keep provider-specific wire mappers separate.
- Preserve abort-on-drop, panic conversion, timeout, and bounded error semantics.

## Acceptance Criteria

- Both providers use one tested transport shell.
- Dropped streams still abort network tasks promptly.
- Provider-specific fixtures remain unchanged.
