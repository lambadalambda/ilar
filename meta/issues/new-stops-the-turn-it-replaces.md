# /new stops the turn it replaces

## Summary

`Driver::close` (driver.rs:534-541) says "whatever the seat was
running is stopped first" but only shuts the spawner and services;
`turn_cancel` is never cancelled. Send `/new` while a turn runs, or
while a 🔑 ask stands: the reply is "Started a fresh chat …", the old
turn keeps running on the removed seat, `/grant`, `/deny` and
`/abort` all say nothing is waiting or running, and minutes later the
old conversation's "denied (no answer)", its final answer or "That
turn failed" land in the fresh chat.

## Requirements

- `close` cancels the turn (the ask closes with "That ask is over",
  the turn ends "Aborted.") before the route is unbound, or `/new`
  refuses while a turn runs and says so.
- A test: `/new` during a turn leaves nothing to arrive later.

Size: S. Source: UX sweep 2026-09-15, gateway.
