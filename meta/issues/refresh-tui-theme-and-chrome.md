# Refresh TUI theme and chrome

## Summary

Borrow btop's restrained contrast and border hierarchy while preserving ilar's
transcript-first layout and the user's terminal background.

## Requirements

- Centralize semantic colors for primary text, metadata, borders, focus,
  running, success, waiting, reasoning, and failure.
- Render transcript body text neutrally while retaining colored identity labels.
- Give transcript, todo, input, and modal borders distinct visual weights.
- Move prompt key hints from the input title to a right-aligned bottom title.
- Reuse one selected-row style across command, model, and reasoning pickers.
- Preserve terminal transparency and avoid forcing a canvas background.

## Acceptance Criteria

- Reasoning is visually distinct from warnings and waiting states.
- Focused input and modal borders are clearer than decorative panel borders.
- Assistant and user body text use primary foreground rather than identity color.
- Existing narrow-terminal rendering remains bounded.
- TUI and workspace checks pass.

## Notes

- Inspiration: btop's near-black, high-contrast visual grammar, not its dense
  dashboard layout.
- Do not add more permanent panels, decorative gradients, or inverted live
  transcript rows.
- Color must remain redundant with labels, words, or state glyphs.
