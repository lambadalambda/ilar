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
