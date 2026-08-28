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

## Requirements

- Find why the slot outlives the conversation. Candidates, in the
  order they are worth checking: a `TurnDone` that never reaches
  the app, and a turn future that never resolves because of
  post-turn work (compaction, checkpointing, spawner or service
  shutdown) or an await tied to a background child that is still
  running. Topic titling is explicitly detached and is not it.
  A testable prediction for the last candidate: the slot frees when
  the *other* builder finishes.
- Whatever the cause, a held slot must not silently swallow
  completions. Either routing stops depending on a slot the model
  has already left, or the slot's release is made unconditional.
- A root stall watchdog (its own issue) would have surfaced this
  instead of leaving it silent for eight minutes; it does not fix
  the cause.

## Acceptance Criteria

- A turn that ends with `end_turn` releases its slot, and a
  completion that lands afterwards starts its follow-up turn. A
  regression test that ends a turn while a background task is still
  running and asserts the next completion routes.

## Milestone

13 — Guard rails
