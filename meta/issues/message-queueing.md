# Message queueing during turns

## Summary

Enter is inert while a turn runs; composing during long turns ends in
waiting. Queue submitted messages and auto-send when the turn completes.

## Requirements

- Enter with a non-blank input during an active turn queues the message
  and clears the input; multiple messages queue in order.
- Queued count is visible near the input; queued text is not yet part of
  the transcript.
- On successful turn completion the next queued message auto-submits
  (unless notifications are paused). On error/abort the queue is kept and
  a notice says so.
- Esc on a blank input with a non-empty queue drops the queue (notice).

## Acceptance Criteria

- Tests: queueing order, auto-send on completion, retention on error,
  Esc clearing.
