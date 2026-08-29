# A steer confirms exactly once

## Summary

`publish()` checks the cancel token first, so a cancellation landing
between the steer's `UserMessage` append and the `Steered` publish
(turn.rs:1661-1669) eats the confirmation. The TUI treats a steer
without `Steered` as undelivered and requeues it: the model reads
the same message twice, the transcript shows it twice.

## Fix

Make `Steered` non-droppable (bypass the cancel gate for
already-appended facts), or key the requeue on the session log
rather than the event.

Size: S-M. Source: sweep 2026-08-29, core loop.
