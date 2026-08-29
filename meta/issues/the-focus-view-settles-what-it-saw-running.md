# The focus view settles what it saw running

## Summary

Focusing a live agent seeds through the restore path, whose last act
marks every still-running tool failed (session_view.rs:383-393) —
right for a dead session, a lie for a live one: click into a child
mid-`cargo test` and the executing tool renders ✗. Worse, the real
`ToolFinished` then lands on a Failed row, which `finish_tool_row`
refuses to settle (transcript.rs:1174-1177) and the synth-row
fallback skips because the id exists — the result is silently
dropped and the row lies until refocus.

## Fix

Skip the failed-marking when seeding a session that is running, or
let the focus fold settle Failed rows. Defeats an acceptance
criterion of [[focus-seeds-the-step-in-flight]]; fix together.

Size: S-M. Source: sweep 2026-08-29, rendering.

## Outcome

The restore path takes a `Liveness`: `Settled` still marks every open
tool row failed, `Running` leaves them open, and the focus view asks
for `Running` only when the agent's events will actually arrive — a
roster row marked *delivering* is a routed completion that publishes no
activity, so it seeds settled. What the seed leaves open, the focused
session's `TurnDone` closes (`close_running_tools` over the focus
lines), so a row can neither lie nor spin forever.
