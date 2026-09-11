# The chat can compact its conversation

## Summary

The TUI has `/compact`; a chat did not, and a long Lemonade
conversation had no way to shed its context short of `/new`.

## Requirements

- `/compact` replaces the chat's conversation with one handover
  summary through the core's manual compaction, waiting for a running
  turn; the summary goes to the daily note; the chat is told the
  handover's size, or that there was nothing to compact.

## Acceptance Criteria

- Test: `/compact` after a turn records a compaction event and a
  daily note, and the chat goes on from the handover.

## Notes

- Done 2026-09-11.
