# A busy child is not a stall

## Summary

A background task is stopped after `stall_timeout` (600s) without an
event on its own loop channel. A foreground `task` call blocks that
turn, so the channel is silent for as long as the child runs — however
busy the child and its descendants are. On 2026-09-07 the root's
background task "Resume Sol browser Wasm" was killed twice this way
(07:56:13 and 08:11:38) while its foreground build child was inside an
explore grandchild that had made 91 tool calls, the last one 2½ minutes
before the kill. The cancellation cascaded down and every level
reported "subagent aborted"; the second run was cut mid-implementation
with no closing message. The parent had already started phrasing its
briefs as "in this call" to fit under the timer.

## Requirements

- Any loop event of a foreground descendant counts as progress for the
  background ancestor's stall watchdog, at any nesting depth.
- A background task whose subtree is genuinely silent still stalls
  after the same timeout.

## Acceptance Criteria

- A test: a background task whose only work is a foreground child that
  keeps emitting for longer than the stall timeout completes normally;
  the existing silent-child test still fires.

## Notes

- The watchdog reads `last_activity` in `crates/ilar/src/subagent.rs`
  from the task's own `rx_evt` watcher only; the foreground path
  publishes child events to the activity channel, not to any ancestor
  timer.
