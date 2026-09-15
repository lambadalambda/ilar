# A propagated result retires its origin

## Summary

Seen on the user's Mac, 2026-09-15: one "Nested task "Review
internal/hub package" failed: its workspace could not be restored"
notification delivered to root session 12cbace5 six times, once per
open, on 9/12, 9/13, 9/14 and 9/15, and the model saying each time
that it had already dealt with it. The origin: outbox/1bf60d67.jsonl
holds the grandchild's completed result, addressed to task session
1bf60d67, whose git worktree `~/repos/ilar-task-hub` no longer
exists. Every open adopts it (its parent chain reaches the root),
routes it, fails to restore the workspace, and `workspace_route_
failure` (subagent.rs) returns `Propagate` with a fresh failure note
for the grandparent. The TUI's `Disposition::Propagate` arm
(schedule.rs) carries that note on and never retires the original,
where `Salvage` and `Exhausted` do; the gateway's arm (driver.rs
~1026) does the same. So the origin stays undelivered for ever and
the failure is manufactured anew each time. The propagated note also
drops the child's actual result: the work is lost, only the plumbing
error climbs.

## Requirements

- When a routed delivery ends in `Propagate` because the target
  cannot take it terminally (workspace gone, context unloadable), the
  propagated notification carries the original result text inside
  its envelope, so the grandparent receives the work, not only the
  error.
- The origin entry is retired once the propagated note is recorded
  for the next hop (before or after delivery, but never left):
  `Disposition::Propagate` in both drivers, and `Exhausted`, retire
  the parcel's original notification. A `Propagate` that is the
  normal climb of a delivered nested result (the child's log did take
  it) needs no retire: delivery to the child's log already counts.
  Say in the code which case is which.
- Adoption never re-adopts an entry whose target session's workspace
  is gone: `pending` (or the driver right after it) treats such an
  entry as routable only once; the retire above is what makes that
  true, and a test proves the second open is silent.
- Tests: a root with a child task whose worktree directory is removed
  and a grandchild completion in the outbox: first open delivers one
  failure-with-result to the root; second open delivers nothing and
  the outbox is empty. Cover both drivers' disposition arms.

Size: S. Source: user report 2026-09-15 ("phantom task results").
