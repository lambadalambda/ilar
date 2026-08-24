# Asides run beside the turn

## Summary

`/btw` is refused while a turn runs ("wait for the current
operation") — but mid-turn is exactly when an aside is wanted, and the
design's whole point is that `aside::ask` is read-only with no writer
lease. Nothing about it needs the turn slot.

## Requirements

- `/btw <question>` works any time. Mid-turn it runs as a detached
  side task (its own handle, like topic titling), in parallel with the
  turn; the turn's busy state, queue and steering are untouched.
- The mid-turn transcript may end with an unpaired tool call (results
  not yet appended). The aside request must cut back to the last
  settled point, or the provider rejects it.
- The answer opens in the same modal; failures are a notice. Nothing
  is recorded, same as today.

## Acceptance Criteria

- A test pins that an aside over a log ending in unpaired tool calls
  sends a settled transcript.
- A test pins that a mid-turn `/btw` neither steers the turn nor
  queues, and its completion touches neither the queue nor the turn
  state.
- The full suite passes.

## Notes

- This removes the aside from the turn-completion plumbing
  (`TurnCompletion::Aside`, the settle gate) in favour of a detached
  handle polled like `topic_handle`. The queue-release dance from the
  original implementation becomes moot: an aside no longer occupies
  the slot anything could queue behind.

## Milestone

11 — Beyond the terminal

## Outcome

Done as specified. `/btw` returns its own `Intent::Aside` from the
submit decision — mid-turn included, where it previously would have
been steering text — and the runtime spawns it on a detached
`aside_handle` polled like topic titling. The turn slot, busy state,
queue and steering are untouched, which the tests pin. A newer aside
cancels a still-running one: newest question wins, and a superseded
answer is silence rather than a modal.

Core-side, `aside::settled` cuts the transcript back past a trailing
assistant message whose tool calls have no results yet, so a mid-turn
snapshot is a valid request. `TurnCompletion::Aside`, the schedule
completion arm and its queue-release dance are deleted — an aside no
longer occupies anything a message could queue behind.

Verified live: an aside answered ("17.") in a modal floating over a
still-streaming turn, the turn finishing underneath, the question in
no session file.
