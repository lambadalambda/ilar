# A torn index still persists the step

## Summary

`ToolCallStarted` handling does file IO through
`contains_tool_call_id(&id)?` (turn.rs:1791); an IO error there
propagates raw — skipping `persist_failed_step`, unlike every
sibling failure. Streamed text the user watched is never persisted;
announced tools never finish; no `TurnDone` is published.

## Fix

Route the error into the `errored` path like the duplicate-id
branch above it.

Size: S. Source: sweep 2026-08-29, core loop.

## Outcome

The duplicate check's IO error goes into `errored` like every sibling
failure, so the step it interrupted is persisted, the announced tools
close, and `TurnDone` is published. The short-circuit on
`seen_tool_call_ids` is kept — that check touches no file — and the
error is not marked retryable: a store that cannot be read is not a
provider hiccup.
