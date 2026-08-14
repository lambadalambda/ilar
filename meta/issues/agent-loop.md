# Agent loop (turn state machine over event channel)

## Summary

The core: consume user input, call provider, stream events, dispatch tool
calls through the barrier executor, append everything to the session,
repeat until turn completes without tool calls. Esc = full abort.

## Requirements

- Async fn per turn, driven by `tokio::sync::mpsc<LoopEvent>` for UI
  forwarding (stream deltas, tool started/finished, turn done).
- Tool results appended as neutral messages; loop continues to next
  provider call while the turn ended with tool calls.
- Stop conditions: no tool calls, max iterations guard, abort token,
  provider error (surface, don't crash).
- Full abort semantics: abort stream + cancel running tools; partial
  transcript stays in session.
- System prompt assembly point (config + AGENTS.md later; loop takes it
  as input).
- TDD entirely against MockProvider + mock tools: multi-turn tool
  conversation scripted end-to-end.

## Acceptance Criteria

- Scripted test: turn 1 emits 2 tool calls (parallel), executor runs
  them, turn 2 finishes with text; session log contains the full
  exchange in order.
- Abort mid-stream leaves a resumable session.
- Loop is provider-agnostic (only ever sees the trait).

## Notes

- Loop must not own the TUI; it publishes events. Keep it a pure-ish
  state machine for testability.
- Max iterations default ~50, configurable.

## Milestone

1 — Lean core
