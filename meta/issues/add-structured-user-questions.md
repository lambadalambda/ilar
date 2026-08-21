# Add structured user questions

## Summary

Add an interactive `question` tool that lets the model request one or more single-choice, multiple-choice, or free-text answers and resume the agent after the user responds.

## Requirements

- Accept multiple questions per tool call with stable question and option IDs.
- Support single-choice, multiple-choice, and free-text questions.
- Support required answers, optional descriptions, and custom “other” text for choice questions.
- Validate requests and responses before they reach the provider transcript.
- Suspend outside ordinary tool execution so waiting does not retain executor or workspace permits.
- Render and operate the request as an interactive TUI modal, including keyboard navigation, text entry, submission, and explicit cancellation.
- Persist requests and answers through ordinary tool-call/tool-result session records.
- Preserve and restore an unanswered question after process restart.
- Keep the core protocol usable by non-TUI embedders through a typed request/response API.
- Restrict questions to the root agent and require a question call to be the only tool call in its provider step.

## Acceptance Criteria

- The model receives a documented `question` tool schema.
- A live turn pauses for a structured answer, then sends a validated tool result to the next provider step.
- Single-choice, multiple-choice, free-text, multiple-question, custom-answer, and cancel flows are covered by tests.
- A pending question can be loaded from a session, answered or cancelled, and resumed without inserting a new user message.
- Waiting for an answer holds no workspace lease or ordinary tool executor task.
- Mixed question/tool batches are rejected before side effects execute.
- `cargo test`, `cargo clippy --workspace`, and `cargo fmt --check` pass.

## Notes

- Use existing assistant `ToolCall` and `ToolResult` session events rather than adding a second persistence representation.
- The core should expose typed question DTOs and answer channels; the TUI is one consumer of that protocol.
