# A message during a turn steers it

## Summary

A message that arrives while a chat's turn runs waits on the turn
lock and becomes the next turn; the running one never sees it. The
TUI steers instead: the loop takes the message at the next step
boundary, and a steer arriving as the model stops reopens the turn.
The core supports it; the gateway passes no steer channel.

## Requirements

- While a turn runs on a seat, an ordinary message on that chat is
  handed to the turn as a steer, with its attachments. A slash
  command is still answered by the gateway at once.
- The status line shows "steered: …" when the model receives it.
- A steer the turn never delivered (it failed or was cancelled) runs
  as a turn of its own afterwards; nothing is lost.
- A message arriving as the turn ends falls back to the next turn.

## Acceptance Criteria

- Tests: a message during a slow tool call reaches the model inside
  the same turn, with one reply; a steer left over from a failed turn
  runs afterwards.
