# Serialize turns and route notifications

## Summary

The TUI can launch multiple notification-driven turns against one session and ignores notification parent ownership.

## Requirements

- Execute at most one turn per session at a time.
- Queue notification-driven turns instead of overwriting active handles and event channels.
- Route notifications to their declared parent session.
- Launch an inactive parent with its persisted provider/model and propagate its resulting completion upward exactly once.
- Preserve queued work when a foreground turn is active or cancelled.

## Acceptance Criteria

- A burst of two notifications executes sequentially without losing either handle or event stream.
- A notification arriving during a foreground turn remains queued.
- Nested notifications are delivered only to their parent session.
- Cancellation still targets the active turn.
