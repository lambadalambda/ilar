# Add command palette

## Summary

Add an extensible command palette opened with Ctrl-P, initially exposing model
selection and leaving room for session switching and other commands.

## Requirements

- Open a centered command palette with Ctrl-P while the TUI is idle.
- Support search, keyboard navigation, Enter selection, and Escape dismissal.
- Include a Switch model command that opens the existing model picker.
- Preserve existing model-picker shortcuts and narrow-terminal behavior.
- Keep command definitions separate from palette interaction and rendering.

## Acceptance Criteria

- Ctrl-P opens the palette without modifying the prompt.
- Selecting Switch model transitions to the existing model picker.
- Search and dismissal behave predictably.
- The palette remains bounded on narrow terminals.
- TUI and workspace checks pass.
