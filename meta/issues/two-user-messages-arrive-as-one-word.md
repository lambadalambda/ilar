# Two user messages arrive as one word

## Summary

`push_user_blocks` merges adjacent user events into one `ChatMessage`
with two `Text` blocks, which is what makes consecutive user messages
safe on the wire. The providers then concatenate adjacent `Text` blocks
with nothing between them:

    …</task-notification>fix the bug

The neighbouring arms in the same function already know better: the
image arm prepends a newline, and the thinking arm inserts a blank line
"rather than glued into one word". The text arm does not.

Every path that writes two consecutive user messages hits this:
compaction, topic, aside, a delivered task notification, and the
salvage added 2026-09-20.

## Requirements

- Adjacent `Text` blocks in one user message are separated on the wire.

## Acceptance Criteria

- A test builds a session with two consecutive user messages and
  asserts the rendered request does not run them together.
- Both provider flavours are covered, since each concatenates in its
  own function.

## Notes

- Found reviewing the salvage-persistence change, 2026-09-20.
- Size: S. Cosmetic in effect but it is the model's input, and a model
  that reads `</task-notification>fix the bug` has been handed a token
  sequence nothing in the training data contains.
