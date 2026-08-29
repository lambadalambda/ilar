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

## Outcome

`publish` reports whether the event reached the reader, and the steer
loop publishes `Steered` *before* appending the `UserMessage`, breaking
without an append when the publish was dropped. The two agree again:
no confirmation means no record, so the reader requeues exactly the
steers the model never saw. The remaining window is one synchronous
append with no await in it — an append failure now loses a steer where
it used to duplicate one, and it fails the turn loudly.
