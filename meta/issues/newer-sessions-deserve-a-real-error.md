# Newer sessions deserve a real error

## Summary

`SessionEvent` is `#[serde(tag = "type")]` with no unknown-variant
tolerance, and `parse_event_bytes` (store.rs:757-762) turns any
unrecognized `type` into a hard `InvalidData` "malformed line N"
that refuses the whole session. A session touched by a newer ilar
becomes unopenable in an older one with an error that reads as
corruption; there is no version field in `SessionMeta` to diagnose
it. Unknown *fields* are already tolerated — this is specifically
new event kinds.

## Requirements

- Distinguish "unknown event type — written by a newer ilar?" from
  genuine corruption in the error message (fail-closed is fine; the
  diagnosis must not lie).

## Acceptance Criteria

- A test: a line with `"type":"from_the_future"` yields an error
  naming the unknown type, not "malformed".

## Milestone

12 — Health sweep
