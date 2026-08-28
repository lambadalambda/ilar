# Results arrive like steers

## Summary

Observed live 2026-08-28 after deliveries were detached from the
turn slot. A child's helper finished; the delivery sat in
"delivering a task result to <uuid>" for many minutes because the
target child was mid-turn and a delivery can only append its result
as a fresh turn — it waits on the session claim for as long as the
current turn runs. Meanwhile the status notice, set once at
delivery start, read like activity while actually meaning "queued
behind a claim". And the agents panel told none of it: the child
being resumed by a delivery has no row (deliveries never register a
task), while a grandchild it spawned does have one — unattributed,
so it reads as the root's own agent.

The root's model shares the blindness: the `tasks` tool reads the
same registry, so it messaged a child that looked idle, raced the
delivery's claim, and lost.

## Requirements

- Steer-first delivery: a completion for a session with a live,
  steerable turn is steered into that turn — the way the user's own
  mid-turn messages arrive — instead of waiting for the turn to
  end. Fallback to the claim-and-fresh-turn path only when the
  target is idle or unsteerable (e.g. itself being resumed by a
  delivery). This mostly removes the waiting state and shrinks the
  task_message race window.
- The live registry matches the durable tree: delivery-driven
  resumes register like task-driven ones, and rows carry the child
  session id and parentage, so the panel and the `tasks` tool see
  every live turn and whose child it is.
- A result that must wait wears a mail badge on its target's row in
  the agents panel — "a message waits for this agent" — instead of
  narrating a stale status line. The transient "delivering" notice
  stays only for the momentary case.

## Acceptance Criteria

- A completion for a busy child lands mid-turn as a steer; the
  child's log shows it arriving inside the running turn.
- The panel shows a row for a session being resumed by a delivery,
  attributed to its parent; a waiting result shows as a badge on
  the target's row.

## The root too

Decided while implementing: the root is an agent like any other and
gets its results the same way — a completion arriving while the
root's turn runs is steered into that turn on the user-steer rails
(pending-tracked, spliced to the queue if the turn never reads it).
Fresh-turn delivery remains for the idle case; a burst while idle
starts one turn and steers the rest into it.

## Open: whole mail, or a mailbox

The whole result is steered in today. The alternative — a ping
("task X finished") plus a mailbox tool to fetch the body — does
not save context in the common case, since the agent will fetch
what it was waiting for; what it buys is flow control under high
fan-in, where many results flooding a focused turn is real. The
outbox is already the storage a mailbox tool would read. If the
flood case shows up in practice, the natural shape is a
count/size-triggered downgrade — full mail normally, "N more
results waiting, fetch when ready" past a threshold — not a mode.

## Milestone

13 — Guard rails

## Outcome

Steer-first everywhere. `route_notification` hands the text to a
live steerable turn before any workspace or claim work — the
exactly-once promise rides ChildSteers (taken steers append,
untaken ones wait pending for the next resume) with the outbox
underneath for process death. The root joined the same contract:
a same-session completion mid-turn is steered on the user-steer
rails (pending-tracked, spliced to the queue if unread), and a
burst while idle starts one turn and steers the rest into it. The
recorded queue-inversion invariant survives in updated form — a
queued user message still outranks a notification for the slot;
the notification now rides that turn instead of waiting.

Visibility: a fresh-turn delivery registers a RunningTask row
(removed by unique row id now, not session id — a delivery and
the turn it waits for share a session), so the panel shows ✉ with
"delivering", foreign-parent rows carry "· for <parent>", and the
tasks tool says "running (receiving a task result)" instead of
letting the model trip over an invisible claim.

Traded consciously: an unconsumed steer means the grandparent
hears through the driving turn's own report rather than a
propagated notification — pending mail is visible in the tasks
listing and durable in the outbox. The whole-mail-vs-mailbox
question stays open above; the outbox is the storage a mailbox
tool would read if fan-in flooding ever bites.
