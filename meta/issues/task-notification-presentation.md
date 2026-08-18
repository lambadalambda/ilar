# Task notification presentation

## Summary

Background task completions are provider-compatible synthetic user messages, but the TUI misleadingly renders them as if the human sent them and exposes internal wrapper tags.

## Requirements

- Render task completions with a distinct `task` attribution instead of `you`.
- Hide `<task-notification>` and `<result>` transport wrappers from the transcript.
- Preserve synthetic user-message semantics in provider requests and persisted sessions.
- Restore historical task notifications with the same presentation.

## Acceptance Criteria

- Live task completion rows are visibly attributed to `task`.
- Resuming a session does not turn task completions back into human messages.
- Normal user messages remain unchanged.
