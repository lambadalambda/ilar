# Mouse motion events drain one frame each

## Summary

`EnableMouseCapture` turns on all-motion tracking (mode 1003), so
moving the pointer across the terminal emits a `Moved` event per cell.
The loop dispatches one terminal event per iteration with a full
`present()` between each, and `Moved` falls into the ignore arm — a
quick sweep queues dozens of events that each cost a frame, and a
click arriving behind them waits its turn. The wheel already solved
this shape with `drain_wheel_batch`; motion needs the same coalescing.

## Requirements

- Consecutive `Moved` events collapse into one, keeping only the
  newest position; the first non-motion event in the batch is deferred
  to the next iteration, exactly like the wheel batch.
- The coalescing is bounded and testable in isolation.

## Acceptance Criteria

- A test that a run of motion events yields the final position and
  defers the first non-motion event.
- A click issued after a pointer sweep is dispatched on the next
  frame, not after a backlog.

## Milestone

11 — Beyond the terminal

## Outcome

`drain_motion_batch` mirrors the wheel batch: a run of `Moved` events
collapses to its newest position, the first non-motion event is
deferred to the next iteration, and the position feeds
`App::update_hover`. Coalescing is pinned by a unit test; the position
now also powers the hover affordance
([hover-underlines-clickable-rows](hover-underlines-clickable-rows.md)).
