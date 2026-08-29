# TurnDone is the last word

## Summary

`publish_terminal` never clears the staged progress/tail maps, and
`try_recv` serves them even after `Disconnected`
(agent/event.rs:195-215, 260-297): a consumer can receive
`ToolOutputTail` for a turn that already reported `TurnDone`,
re-animating a finished row.

## Fix

Clear both maps in `publish_terminal`, or gate the receiver after a
terminal event.

Size: S. Source: sweep 2026-08-29, core loop.

## Outcome

`publish_terminal` clears the staged progress and tail maps, and the
receiver keeps a `finished` flag — set in `settle`, which both `recv`
and `try_recv` route through — that stops `next_progress` for good.
One agent-loop test that swept the channel *after* the turn now drains
concurrently with it (`tokio::join!`), which is what a live consumer
does anyway.
