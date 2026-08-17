# Select and copy transcript text

## Summary

Allow users to select rendered transcript text with the mouse and automatically copy the selection to the system clipboard.

## Requirements

- Support click-drag selection across visible transcript rows, including wrapped text.
- Copy selected text automatically when the mouse button is released.
- Preserve transcript scrolling and normal keyboard input behavior.
- Provide visible selection feedback without corrupting transcript styling.

## Acceptance Criteria

- Dragging over output selects the corresponding rendered text.
- Releasing a non-empty selection writes it to the clipboard.
- Multiline and reverse-direction selections copy in display order.
- Mouse-wheel scrolling and existing TUI tests continue to work.
