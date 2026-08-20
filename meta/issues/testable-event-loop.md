# Make the event loop testable

## Summary

`run_app` in `crates/ilar-tui/src/main.rs` is ~980 lines of `match` over
terminal events, with the decision and the effect fused in every arm:
each one reads `App`, mutates it, spawns turns, and posts synthetic
events inline. Nothing can observe a decision without running the loop,
and the loop needs a terminal, a provider and a session store.

The cost shows up as untested behaviour, not as awkward code. Two
acceptance criteria in
[Unify the TUI modal layer](unify-tui-modals.md) are marked verified by
reading rather than by test, purely because both live inside `run_app`:
paste routing to the search query, and the notification gate refusing to
start a turn while an overlay owns the keyboard. The same gap hid the
queue/goal interaction bugs recorded in DEVLOG for Milestone 5, which
were caught by review rather than by tests.

Splitting main.rs did not fix this and was never going to: moving a
function does not make it testable. What is needed is the decision logic
lifted out of the loop.

## Requirements

- A pure step function mapping (event, observable app state) to an
  intent, with no I/O and no `App` mutation: something like
  `fn decide(event: &Event, state: &LoopState) -> Vec<Intent>`.
- `run_app` keeps only the effectful half — interpreting intents,
  spawning turns, draining channels, drawing.
- Intents cover the cases that currently have no coverage: routing a
  paste, gating a notification, dequeuing a queued message, continuing a
  goal round, arming and firing a retry.
- Synthetic events (the `pending_terminal_event` re-entry used by retry,
  goal continuation and queue drain) become explicit intents rather than
  a fake keypress fed back through the dispatcher.

## Acceptance Criteria

- Table-driven tests over `decide` covering, at minimum: paste while
  search is open goes to the query; a notification does not start a turn
  while a modal is active; a queued message is only dequeued into an
  idle, modal-free, draft-free input; a goal round does not continue
  when any of those hold.
- The two criteria currently marked "verified by reading" in
  [Unify the TUI modal layer](unify-tui-modals.md) become real tests.
- No behaviour change: the full suite passes before and after.

## Notes

- This is a behavioural refactor, not a move, so it should not be
  attempted the way the module split was. Land it in small steps with
  the existing tests as the safety net.
- The dispatcher is also still an `if app.active_modal() == Some(..)`
  chain rather than an exhaustive `match`, so a new `Modal` variant
  compiles with no arm. Worth fixing in the same pass, since both are
  about the loop's shape.

## Milestone

6 — Hardening
