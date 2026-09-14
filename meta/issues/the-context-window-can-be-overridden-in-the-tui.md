# The context window can be overridden in the TUI

## Summary

A discovered or local model often reports a window the session
cannot use; the person knows better and had no way to say so.

## Requirements

- `/context` opens a picker of common sizes (model default, 32k, 64k,
  128k, 200k, 256k, 512k, 1M); `/context <size|default>` sets one
  directly; the palette offers "Set context window".
- The override is the session's: it drives the footer's ctx meter and
  the turn's compaction threshold, survives a model switch, and is
  not persisted.

## Acceptance Criteria

- Tests for the size parser, the command routing (including mid-turn),
  the picker and the effective limit.

## Notes

- Done 2026-09-14.
