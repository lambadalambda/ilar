# A share carries its delegations

## Summary

A shared HTML file carries one session. Opening a `task` row in it
says "this delegation's own transcript is not in this file" — honest,
and less than [[a-session-shares-as-one-html-file]] promised.

On the server a Task row fetches two more routes:
`/api/sessions/{id}/invocations/{callId}` and
`/api/sessions/{childId}?invocation={callId}`. The share's embedded
route table carries neither.

## Requirements

- A delegation opens in a shared file and shows the child's work, as
  it does on the server and in the terminal.
- The child's results are redacted and bulk-cut the same way the
  parent's are.

## Acceptance Criteria

- A test seeds a parent with a subagent and asserts the child's words
  are in the written file.
- The browser check opens a task row and sees the timeline.

## Notes

- Found reviewing the share feature, 2026-09-21, against its own
  acceptance criteria.
- Size: S–M. The payload builder already loops `children_of`; what it
  needs is each child's projected page under the two keys the page
  builds, which is where the care is — the key must match
  `encodeURIComponent` byte for byte, as `urlish` already does.
- The sidebar's child rows and the "open the child session" link set a
  hash that `routeId()` ignores in share mode, so they do nothing.
  Either make them work or take them out.
