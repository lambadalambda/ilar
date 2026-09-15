# The weekly review stays private

## Summary

`LAST_ACTIVE` resolves to `routes.last_active` with no group check
(gateway.rs:824-834; `touch` records groups too, routes.rs:692-698).
The background seat for a group is non-private, which withholds only
the core block; the `memory`, `memory_search` and `memory_get` tools
are added regardless (driver.rs:346-360), and the weekly PROMPT tells
the model to read `memory/daily` and "Send one short message with
what you changed" (weekly.rs:1133-1141). If the last message before
Monday 04:00 UTC was in a group, the room gets a summary of the
person's memory. The after-turn review is gated on `seat.private`
(gateway.rs:885); the weekly one is not.

## Requirements

- `LAST_ACTIVE` skips groups, or the weekly job targets the last
  private chat.
- Memory tools are withheld from non-private seats.
- A test for both.

Size: S. Source: UX sweep 2026-09-15, gateway.
