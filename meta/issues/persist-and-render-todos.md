# Persist and render todos

## Summary

Todo state is memory-only, disappears on resume, and is not visible in the TUI beyond generic tool completion lines.

## Requirements

- Persist todo updates as session state.
- Replay todo state on resume.
- Retain shared todo state in the TUI and render it compactly.
- Consume the deterministic call-order guarantee from tool scheduling.

## Acceptance Criteria

- Restarting and resuming restores the latest todo list.
- TUI tests show current pending, active, and completed items.
