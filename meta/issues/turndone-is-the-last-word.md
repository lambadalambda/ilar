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
