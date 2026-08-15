# Fix z.ai OpenAI wire format

## Summary

The OpenAI-compatible z.ai flavor emits an empty user message before tool results and sends the system prompt in a nonstandard field.

## Requirements

- Emit tool-result messages directly after assistant tool calls.
- Emit user messages only when user text exists.
- Serialize the system prompt as a system-role message.
- Preserve valid tool-call ordering across multiple results.

## Acceptance Criteria

- Wire tests cover system prompts and a full assistant-tool-result continuation.
- No empty user message appears before tool results.
- Payload tests assert `stream_options.include_usage` remains enabled.
