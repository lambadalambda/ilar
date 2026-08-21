# Grow the input box for long messages

## Summary

Wrap long input text and grow the composer so the whole message remains visible while editing.

## Requirements

- Wrap input text to the available composer width.
- Increase the input box height to display the complete message.
- Preserve the existing minimum input height and surrounding layout behavior.

## Acceptance Criteria

- A render test demonstrates that a long input wraps across multiple visible lines.
- The full wrapped message is visible without clipping.
- Existing TUI tests remain green.
