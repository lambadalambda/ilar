# TUI resume, input, and status

## Summary

Resumed sessions open with an empty transcript, input editing is append-only, and status loses useful model and usage information.

## Requirements

- Validate and render resumed transcripts before entering raw mode.
- Restore persisted agent/model context.
- Add cursor-aware editing, paste handling, and a deliberate multiline/send binding.
- Preserve model and latest usage in idle status.
- Virtualize transcript scrolling beyond the Paragraph u16 offset.

## Acceptance Criteria

- Resume immediately displays prior messages and rejects invalid IDs at startup.
- Users can move the cursor, edit in place, and paste multiline prompts.
- Sending versus inserting a newline is documented and tested.
- Very long transcripts remain scrollable to the true tail.
