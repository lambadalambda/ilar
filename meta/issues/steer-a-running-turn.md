# Steer a running turn

## Summary

A message typed while a turn is running is queued, and the queue only
drains once the turn is *completely* finished. `run_app` gates the
dequeue on `TurnOutcome::Completed`, which means "the model stopped
calling tools" — so on a forty-step task the message waits through every
tool round. There is no way to redirect work in flight: the only
alternative is Esc, which throws away everything the turn has done.

opencode solved this and the shape is worth copying. Every admitted
input carries a `delivery` of `"steer"` or `"queue"`
(`packages/core/src/session/input.ts`), and the runner loop
(`packages/core/src/session/runner/llm.ts:383-406`) treats them
differently:

```js
while (shouldRun) {                                 // one pass per queued input
  let needsContinuation = true, step = 1
  while (needsContinuation) {                       // one pass per step
    const result = yield* runTurn(sessionID, promotion, step)
    needsContinuation = result.needsContinuation
    promotion = "steer"                             // every later step promotes steers
    if (!needsContinuation)
      needsContinuation = yield* hasPending("steer") // a steer re-opens a finished turn
  }
  shouldRun = yield* hasPending("queue")            // queue waits for the turn to end
}
```

Steers are promoted into the history *before the next request is built*,
so they reach the model at the next step boundary rather than the next
turn. If the model was about to stop and a steer has arrived, the turn
continues instead of ending. Queue keeps our current semantics: one at a
time, FIFO, only once the turn is genuinely done.

Their client no longer offers the choice. `followup` was a user setting,
and it is now coerced — default `"steer"`, getter and setter both map
`"queue" -> "steer"`, and a `createEffect` rewrites any stored `"queue"`
on load (`packages/app/src/context/settings.tsx:329-353`). They kept
both in the core and retired queue-only from the UI.

## Requirements

- A message submitted during a running turn is delivered to the model at
  the next step boundary, not at the end of the turn.
- Injection happens only at a settled point in the loop: after tool
  results are appended and with `continuations.is_empty() &&
  paused_content.is_empty()`. That is the top of the
  `while iterations < config.max_iterations` loop
  (`crates/ilar/src/agent/turn.rs:927`) — the same place the mid-turn
  compaction check already sits, and for the same reason. Injecting
  between an assistant message carrying tool calls and its results would
  break the pairing.
- A steer arriving as the model finishes re-opens the turn instead of
  ending it: the `!had_tool_calls` early return
  (`turn.rs:1361`) must first check for pending steers.
- Steers outrank queued messages. When a turn starts from the queue,
  promote one queued message plus every pending steer.
- Queue remains available and keeps its current behaviour, including the
  existing guards (idle, modal-free, no draft) and the Ctrl-Q manager.
- The UI distinguishes the two: the input title says which is pending,
  and the notice on submit says what will happen.

## Acceptance Criteria

- Mock test: a steer submitted while the model is mid-tool-loop appears
  in the request for the *next* step, not after the turn ends.
- Mock test: a steer arriving on a step with no tool calls continues the
  turn rather than returning `Completed`.
- Mock test: a queued message still waits for the turn to end, and only
  one promotes per turn.
- Mock test: starting a turn from the queue also drains pending steers.
- Transcript invariant holds after injection: no assistant message with
  tool calls is separated from its results, and role alternation is
  preserved.
- Existing queue tests pass unchanged.

## Notes

- `run_turn` currently owns a one-way channel (`LoopEventSender`, loop
  to UI). Steering needs the reverse — a receiver the TUI can push into
  while the turn runs. That is the main structural piece.
- Decide the default deliberately. opencode's conclusion after shipping
  both is that steer should be the default and queue the exception; our
  current behaviour is their deprecated mode. Changing the default is a
  behaviour change for anyone relying on "it waits until done".
- Two details from their implementation worth keeping: promoting resets
  the step counter, so an interjection gets a fresh step budget instead
  of eating the current one; and steer promotion uses a sequence cutoff
  so a steer arriving mid-promotion is not half-applied.
- Optional, and a real difference: opencode persists admitted inputs in
  SQL, so a queued or steered message survives a restart. Ours live in
  `App::queued_messages` and are lost on exit.
- The queue/steer arms live in `run_app`, which has no test harness —
  see [Make the event loop testable](testable-event-loop.md). The core
  half is testable today through `MockProvider`; the UI half is not.

## Milestone

6 — Hardening
