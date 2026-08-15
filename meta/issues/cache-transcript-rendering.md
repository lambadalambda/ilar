# Cache transcript rendering

## Summary

The TUI reparses, clones, and rewraps the complete transcript on every frame.

## Requirements

- Cache finalized Markdown entries.
- Invalidate only streaming or changed entries.
- Avoid rendering wrapped rows far outside the viewport.

## Acceptance Criteria

- Appending a delta does not reparse finalized messages.
- Long idle transcripts do not allocate a full replacement transcript each frame.
- Visual output and scrolling remain unchanged.
