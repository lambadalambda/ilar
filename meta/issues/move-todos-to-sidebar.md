# Move todos to a sidebar

## Summary

The current todo checklist is appended to the transcript tail, so it moves with conversation output instead of behaving like persistent task state.

## Requirements

- Render the current todo list in a fixed sidebar on the right.
- Keep transcript scrolling independent from todo rendering.
- Preserve compact status styling, overflow handling, and live replacements.
- Provide a usable narrow-terminal fallback without placing todos back in the scrolling transcript.

## Acceptance Criteria

- On normal-width terminals, todos remain visible in a right sidebar while transcript output grows and scrolls.
- Todo updates do not change transcript content height or scroll position.
- Narrow terminals retain usable transcript, status, and input regions.
- Existing persistence, resume, mouse selection, and scrolling tests continue to pass.
