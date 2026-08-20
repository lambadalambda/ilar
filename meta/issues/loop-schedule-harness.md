# A harness for the event loop's schedule

## Summary

`decide()` covers decisions; nothing covers the *schedule* — where the
queue drain sits relative to the notification gate, the steer channel
and the render inside `run_app`'s iteration. Bugs in that ordering have
been the recurring class in this codebase, and none of them were caught
by tests:

- The queue-inversion bug (DEVLOG, Milestone 5, fixed in phase two of
  [Make the event loop testable](testable-event-loop.md)): draining the
  queue posted a synthetic Enter, the notification gate fired first in
  the same iteration, and the drained message was silently converted
  into a steer of the notification's turn.
- The synthetic Enter being swallowed by the Ctrl-X model chord, and a
  queued bare `/rev` eaten by the completion popup — same shape: two
  loop mechanisms firing in an order nobody chose.

All were caught by review, i.e. by reading. A complete
`decide(event, &state) -> Vec<Intent>` would not have seen any of them,
because each decision was individually correct; the composition was not.

## Requirements

- Extract one loop iteration into a function that can be driven without
  a real terminal, provider, or session store: something like
  `tick(app, incoming_event) -> Vec<Effect>`, where the spawn and the
  render are the only things left outside.
- A test can enqueue events (key, paste, turn completion, notification,
  background job exit) and assert on the *sequence* of effects across
  iterations, not just the decision for one event.
- The regression cases above become tests: a dequeue and a notification
  arriving in the same iteration must start the dequeued turn first; a
  queued slash invocation survives every path.

## Acceptance Criteria

- The queue-inversion scenario is reproduced as a failing test against
  the pre-fix ordering (demonstrated by mutation: reordering the drain
  and the gate fails a test).
- At least the three regression cases above are covered.
- No behaviour change: the full suite passes before and after.

## Progress

Phase one landed: the stretch whose ordering caused every recorded bug
— intent drain, palette peek, notification gate — is
`schedule::settle`, driven through a `Runtime` trait. `run_app`
implements it over tokio and crossterm (`LoopRuntime`, holding mutable
borrows of the loop's state so a spawn is observed by the rest of the
pass); tests implement it with a recorder whose `turn_running` flips
when a turn starts, which is exactly what makes a reordering visible.

Acceptance criteria met by it:

- The queue-inversion scenario is a real test
  (`a_dequeued_message_outranks_a_notification_in_the_same_pass`), and
  reordering the gate before the drain inside `settle` fails it —
  verified by performing that mutation.
- A queued slash invocation reaches the runtime expanded; a modal
  holds both the queue and the gate without losing either; an idle
  pass admits the notification (same-session starts a turn, foreign
  routes and restarts the iteration, preserving the gate's old
  `continue`).

Phase two folded the completion in. `schedule::pass(app, completion,
carried_intents, runtime)` is the iteration's spine: completion
bookkeeping and its `after_turn` decisions, then the drain, the
palette peek and the gate — one function, one order. Of the
completion machinery, run_app keeps only the join at the edge
(`handle.await` mapped into a `Completion` value); the subtask spawn,
bell, bookkeeping, render and the whole dispatch half remain outside
the seam. The trait grew the completion edges: pause/resume
notifications, hold propagate/requeue, end_turn, revert_model.

Newly pinned (the first two verified by performing the mutation):

- Completion decides *before* the drain and the gate: folding it in
  after the settle fails
  `a_completion_decides_before_the_drain_and_the_gate`.
- A queued turn starts under the *reverted* model — the revert sits
  between the completion and the drain; deleting it fails
  `a_queued_turn_starts_under_the_reverted_model`.
- An abort resumes nothing (notifications stay paused, the queue
  holds); a completed turn resumes the flow and the freed gate admits
  a waiting notification in the same pass.
- Undelivered steers return to the queue and wait for the user —
  `after_turn` observes before the splice.
- A goal round continues through the whole pass; a requeued routing
  pauses the gate and is held rather than delivered.

Still not covered, so the issue stays open: the render and event-poll
positions, the event dispatch half, and cross-iteration sequences (an
event-half intent surviving into the next pass's drain). The
remaining shape: fold the poll/dispatch half in and run_app is a loop
over `tick` plus I/O.

One deliberate reorder, judged inert: the subtask spawn used to sit
between the drain and the gate and now runs after `settle` (it touches
neither `turn_handle` nor anything the gate observes). It runs before
the `Restart` continue, so a routed notification cannot defer it.

## Notes

- This is deliberately **not** part of Milestone 6. Phase three of
  [Make the event loop testable](testable-event-loop.md) closes that
  milestone's criteria; this issue is the follow-on that covers what
  `decide()` structurally cannot.
- The spawn block needs a provider and a session store; `MockProvider`
  exists on the core side and may be reusable here, which would let the
  harness run whole turns rather than stubbing `turn_running`.
- Expect this to force `run_app` into the shape the name suggests: a
  loop over `tick()` plus I/O at the edges. That is the real payoff —
  the schedule becomes a value that tests can see.

## Milestone

7 — Unscheduled
