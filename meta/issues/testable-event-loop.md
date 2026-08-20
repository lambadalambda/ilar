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

## Progress

Phase one landed: the decisions that had no coverage are extracted into
`crates/ilar-tui/src/decide.rs` as pure functions over one `LoopState`
snapshot — `paste_target`, `submit_target`, `queue_step`, `goal_step`,
`may_route_notification`. Each returns a value and the caller acts, and
`observe()` builds the snapshot the same way at every site so a decision
cannot be handed a plausible default for something the caller did not
compute.

They are shaped to compose: `decide(event, state) -> Vec<Intent>` should
be an assembly of these rather than a rewrite of them.

**What phase one does not buy, stated plainly.** The decision *logic* is
covered; the *wiring* is not. Review demonstrated this by gutting every
call site at once — swapping the paste targets, deleting the
notification gate, making the queue drain a no-op — and the whole suite
stayed green, with the paste swap producing no warning at all. So the
two criteria in [Unify the TUI modal layer](unify-tui-modals.md) are
still verified by reading; what changed is that the reading is three
lines of match arm instead of a forty-line block.

Phase two landed the intent layer. `after_turn` and `retry` return
`Vec<Intent>`; `apply_intent(app, intent) -> Option<String>` owns every
state change and hands back the prompt when a turn should start, so only
the spawn needs the runtime. **The synthetic-Enter trick is gone** — the
queue drain, goal continuation and retry no longer post a fake keypress
and hope the dispatcher is in a state that routes it correctly.

Removing it closed a bug recorded in DEVLOG under Milestone 5: draining
the queue set the synthetic Enter, the notification gate then fired
first, and the drained message was silently converted into a *steer of
the notification's turn*. Two more disappeared with it — the synthetic
Enter could be swallowed by the Ctrl-X model chord, and a queued bare
`/rev` was eaten by the completion popup.

Coverage, measured the same way as phase one. `apply_intent` is pinned:
mutating any of `StartTurn`, `SendQueued`, `ClearGoal` or `AdvanceGoal`
fails a test. Still green under mutation, and therefore still untested:

- the drain loop itself — deleting it entirely leaves the suite green
- the four sites that push intents
- paste routing, the notification gate, and the start/steer/queue branch

So the two criteria in [Unify the TUI modal layer](unify-tui-modals.md)
remain verified by reading after phase two. Phase three is
`decide(event, &state) -> Vec<Intent>` for the key and paste paths.

Phase three landed. Submitting and pasting are decisions now:
`decide::submit(state, busy, text)` returns `StartTurn`/`Steer`/`Queue`
carrying the text, `decide::paste(state, text)` returns the owning
surface's intent (or nothing, for a picker), and `apply_intent` owns
the effects — including the steer-channel send with its fall-back to
the queue when the channel closed mid-submit. Event-side intents apply
immediately via `apply_event_intents` rather than deferring to the
central drain: a steer deferred one tick can miss its turn, and a
queue push deferred past the completion check strands the message.

Coverage, measured by mutation as before. Newly failing under
mutation: the submit and paste routing (swapping the search and input
arms now fails a test — the phase-one gap that produced no warning at
all), the steer fallback (deleting it fails), the paste effects
(deleting `search_refresh` fails), and the queue→send sequence is
composed end to end in `a_message_submitted_mid_turn_queues_and_then_sends`.

Still green under mutation, and therefore the honest boundary of this
issue: the per-event-class call sites in `run_app` (one
`apply_event_intents` line each) and the notification gate's one-line
consultation. Those are the loop's *schedule*, which no `decide()`
test can see — the spawn block likewise needs a provider and a session
store. Both are filed as
[A harness for the event loop's schedule](loop-schedule-harness.md).

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
