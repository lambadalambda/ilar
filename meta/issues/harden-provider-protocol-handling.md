# Harden provider protocol handling

## Summary

Provider mappers silently discard malformed JSON, accept missing required fields, permit critical option overrides, and mishandle pause/truncation states.

## Requirements

- Convert malformed protocol payloads and missing required fields into terminal errors.
- Bound SSE event and parser buffers.
- Reject malformed argument JSON, empty or duplicate tool IDs, duplicate completions, and contradictory stop reasons.
- Reject option keys that override model, input/messages, tools, or streaming.
- Reissue paused turns with cancellation and a finite retry cap.
- Reject truncated null-input tool calls before invoking custom tools.
- Bound and redact HTTP error bodies before persistence.

## Acceptance Criteria

- Malformed JSON cannot produce a successful truncated turn.
- Reserved options are rejected before network I/O.
- Pause and null-input paths have focused agent-loop tests.
- Error body memory and persistence are bounded.
