# Background agents + completion notifications

## Summary

`background: true` on Task: returns immediately, child runs detached,
parent gets a synthetic user message on completion that re-invokes the
loop. The convergent Claude Code/opencode pattern.

## Requirements

- Foreground Task with `background: true` returns "task started" tool
  output immediately with do-not-poll guidance text.
- Child runs detached (tokio spawn + abort handle registry), writes its
  session.
- On completion (or failure): inject synthetic user message
  (`<task-notification>` with summary/result/usage) into parent session
  and kick the parent loop (wakes idle loop).
- Abort of parent kills children (unless detached explicitly later).
- Stall watchdog: if child emits nothing for configurable timeout,
  fail with error notification.
- TDD: mock child completes -> parent receives notification message;
  watchdog fires on silent child.

## Acceptance Criteria

- Mock end-to-end: background task notification re-invokes parent loop
  exactly once.
- Watchdog test with short timeout.

## Notes

- Registry needed: task id -> abort handle + status, queryable by TUI
  later. Keep in core, not TUI.

## Milestone

2 — Multiply
