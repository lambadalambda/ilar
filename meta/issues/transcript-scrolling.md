# Transcript scrolling

## Summary

Long transcripts overflow the viewport without auto-following new output or providing discoverable, correctly directed scrolling.

## Requirements

- Follow the transcript tail by default while content streams.
- Allow keyboard and mouse scrolling through wrapped visual rows.
- Stop auto-following when the user scrolls upward and resume it upon reaching the bottom.
- Show the current scroll state when content exceeds the viewport.
- Clamp correctly after content changes and terminal resizes.

## Acceptance Criteria

- Tests cover tail following, page movement, clamping, and resuming tail follow.
- Page Up moves toward older content and Page Down toward newer content.
- Mouse wheel scrolling works when mouse capture is enabled.
- A scrollbar and compact position indicator appear only for overflowing content.
- Workspace tests, formatting, and clippy pass.
