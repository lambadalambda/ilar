# Robust bash execution

## Summary

Bash can deadlock on stderr, panic while truncating UTF-8, retain unbounded output, and leave descendant processes alive after cancellation.

## Requirements

- Drain stdout and stderr concurrently.
- Enforce bounded retained output while continuing to drain pipes.
- Truncate only at valid UTF-8 boundaries.
- Preserve arbitrary invalid-UTF-8 output lossily instead of discarding diagnostics.
- Run commands in a process group and terminate descendants on timeout or cancellation.
- Preserve useful exit and truncation diagnostics.

## Acceptance Criteria

- Simultaneously full stdout and stderr cannot deadlock.
- Multibyte output at the limit cannot panic.
- Retained memory remains bounded for arbitrarily verbose commands.
- Timeout and dropped futures terminate descendant processes.
