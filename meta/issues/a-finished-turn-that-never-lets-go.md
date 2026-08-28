# A finished turn that never lets go

## Summary

Observed 2026-08-28 with a screenshot and confirmed against the log.
A root turn spawned two background builders, wrote a closing
summary, and ended: the session's last event is an
`assistant_message` with `stop_reason: "end_turn"`, and nothing
follows it. One builder then finished — and its completion never
arrived. The UI meanwhile showed `thinking… · 1.4 KiB · no data
474s`.

A turn appends its prompt *before* calling the provider
(`turn.rs:1291`), so a routed notification would have written a
`<task-notification>` user message. There is none, and this same
session routed one successfully hours earlier, so the mechanism
works in general and simply did not run here.

Routing is gated on `!turn_running`
(`decide.rs::may_route_notification`), and `turn_running` is
`turn_handle.is_some()`. So the conversation was over while the
turn slot was still held, and every background completion queued
behind a turn that had nothing left to say. The stale
`1.4 KiB / no data` reading is the last step's stream state, which
nothing reset — consistent with a `TurnDone` that never landed.

Not the earlier theory: the provider did not hang mid-answer. The
answer completed. What is stuck is whatever comes after it.

## The cause

Found: `main.rs::route` — the path a completion takes when its
parent is *not* the root session — sets `app.busy`, sets the
activity to Thinking, and puts its future in `*self.turn_handle`.
It occupies the root's single conversational turn slot. And
`route_notification` runs with a discarded event sender, so nothing
it does publishes anything: the activity line keeps the previous
step's byte count and its "no data" clock runs on.

So a grandchild finishing makes the root look like it is thinking,
for as long as that child's follow-up turn takes, while showing
numbers left over from a turn that ended an hour ago. And because
`may_route_notification` is gated on `turn_running`, every other
completion — including the root's own children — waits behind it.

The session that prompted this shows it plainly: the root's last
event is 13:41:10, while its two children kept appending until
14:50 and 15:07. Those were routed turns, driven from the root's
slot, writing only to the children's logs.

## Adjacent losses found on the same re-read

Three more ways this pipeline drops completions, all in the same
seam and covered by the same rework:

- `hold_propagate` and `hold_requeue` write into a single
  `Option<PendingNotification>`. A held completion is *overwritten*
  — silently lost — when a second routed turn finishes while the
  first's propagated result still waits behind the channel backlog.
  One slot cannot hold a queue.
- A routing failure (`Completion::Routed(Err)`) prints only the
  error. The child's final text — the work — is discarded with it.
- The gate holds *foreign* completions on the root's own state
  (`turn_running`, open modal). A child's resume needs nothing the
  root holds; during a long root turn every grandchild completion
  queues for no reason.

## Requirements

- A routed turn must not hold the conversational slot. It writes to
  another session entirely, so it belongs with the other detached
  work (asides, topic titling) — which the code already models and
  which `route` explicitly does not follow. Several may then run at
  once, and the user's own turn is not blocked by plumbing.
- Delivery must not be gated on the slot for the same reason: a
  foreign completion routes even while a root turn runs or a modal
  is open. Only the requeue pause still holds everything.
- Held completions live in an ordered queue, not a slot; holding a
  second one must never drop the first.
- A failed routing surfaces the child's final text in the
  transcript, not just the error that kept it from its parent.
- While a routed turn runs, the UI must not present it as the root
  thinking. Either show it for what it is (a delivery in progress)
  or show nothing, but never a stale byte count and a "no data"
  clock from an unrelated turn.
- A session switch and quit must account for running deliveries the
  way they account for background agents.
- The sweep's remaining half is adjacent: a routed turn is still
  invisible from the parent's task row (see the-replay-sweep).

## Acceptance Criteria

- A completion routed to a child does not block a completion
  destined for the root, and does not make the root read as busy.
  Test: two children, one routed completion in flight, assert the
  second still routes.

## Milestone

13 — Guard rails
