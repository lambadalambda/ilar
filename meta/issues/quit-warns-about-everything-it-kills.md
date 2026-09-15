# Quit warns about everything it kills

## Summary

`quit_warning` (app.rs:2210-2229) counts only the stash and
undelivered task results. Ctrl-D on a blank prompt during a turn
exits at once: the turn is cancelled, `spawner.shutdown()`
(main.rs:3524-3531) cancels every background agent (the next open
gets "Task "X" was cancelled." mail), and messages queued during the
turn are gone, while a two-word stash gets a warning. Related:
focus messages in flight are aborted uncounted (filed under
talking-to-a-focused-agent-polish).

## Requirements

- One warning naming the running turn, N background agents, N
  queued messages, N focus messages; the second press quits.

Size: S. Source: UX sweep 2026-09-15, session lifecycle + agents.
