# The phone picks model and thinking

## Summary

A menu in the phone app shows the session's model and thinking level
and changes them, as the TUI's model picker does.

## Requirements

- The wire sends the model list, the current model and the thinking
  levels it allows; the phone sends a choice.
- The TUI applies it as its own picker does, and only while it may
  (not mid-turn if the picker forbids that).

## Acceptance Criteria

- From the phone, switch the model and thinking level; the TUI status
  bar shows the change, and the next turn uses it.
