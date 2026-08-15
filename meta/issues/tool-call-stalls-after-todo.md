# Tool call stalls after todo

## Summary

After the model completes a `todo` tool call, the TUI shows the tool as successful but no further activity occurs. The tool call and result may also be absent from the on-disk JSONL session while the process is running.

## Requirements

- Persist completed assistant tool calls and tool results promptly.
- Continue the provider loop after successful tool execution.
- Surface provider-loop failures in the TUI instead of appearing stalled.

## Acceptance Criteria

- A regression test proves tool-call and tool-result events are visible on disk immediately after append.
- An agent-loop test proves a successful tool call is followed by the next provider request and final response.
- Relevant tests, formatting, and clippy pass.
