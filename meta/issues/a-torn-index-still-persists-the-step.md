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
