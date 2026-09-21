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

## Done (2026-09-21)

A task row in a shared file opens the child's timeline, as it does on
the server. The payload walks the session's children, reads each
child's whole log, and for every invocation it names answers the two
routes the row asks for: the child a call is writing, and that child's
slice for the call. The slice is one page, whole, so the "load earlier"
path the page has for a server never fires. A child whose log cannot
be read stays out and its row still says so, rather than one bad
delegation failing the share.

The child's cut results travel under the child's own results route,
through the same redaction and the same bulk cut as the parent's —
`cut_results` is one function now, called per timeline. Verified in a
real browser (the Playwright chromium shell on tenco, driven over its
debugging protocol): the row opens, the delegate's words are there, and
opening the child's grep row pulls its full result from the file,
which the cut copy could not have supplied.

The dead links are gone: a shared sidebar lists its subagents as plain
rows, and the child timeline has no "open the child session" link,
because there is nowhere to open it.

Not done: a child's own children. Subagents are flat here
(`max_depth = 1`), so a delegation's delegations do not exist to carry.
