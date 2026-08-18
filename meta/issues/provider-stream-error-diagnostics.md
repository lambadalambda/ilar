# Provider stream error diagnostics

## Summary

Terminal provider stream events can surface as `unknown provider error` when their message uses an unrecognized JSON shape, hiding the actionable failure after otherwise successful tool calls.

## Requirements

- Recognize common nested error-message shapes used by provider SSE events.
- Replace the generic fallback with a bounded, sanitized diagnostic containing available error metadata.
- Preserve terminal stream and session-recovery behavior.

## Acceptance Criteria

- OpenAI-style top-level `error.message` events expose their real message.
- Message-less errors expose a useful bounded fallback without leaking secrets.
- Existing provider error fixtures and full verification remain green.
