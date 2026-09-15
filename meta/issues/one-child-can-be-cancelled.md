# One child can be cancelled

## Summary

- There is no way to cancel one child: the panel click focuses
  (sidebar.rs:170-175), Esc leaves the view, and the only cancel is
  Ctrl-Q "background jobs: N running" → `d d`, which runs
  `abort_all()` and cancels deliveries too (main.rs:3652-3657). The
  `/sessions` refusal "background agents are running; wait or abort
  them first" (main.rs:2514) names no key. A per-row cancel (a key
  in the focus view or on the row) and a refusal naming it.
- Held results are a count with no list: "N task result(s) held —
  send a message to deliver" (view.rs:137-141); the pending manager
  has no held item (app.rs:1631-1646). The user cannot see which,
  read them, or deliver without spending a turn. List them in Ctrl-Q
  with a deliver action.
- A result for a child being resumed waits unbounded
  (subagent.rs:2026-2041: no cap on the claim wait), two ✉ rows for
  one session, `/sessions` refused with "a task result is being
  delivered; wait a moment" for the whole other turn. Requeue after
  a short wait as the lease path does. (Unverified beyond reading.)

Size: S-M. Source: UX sweep 2026-09-15, agents.
