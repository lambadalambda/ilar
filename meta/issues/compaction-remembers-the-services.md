# Compaction remembers the services

## Summary

The handover summary asks the model to carry the todo list across a
compaction, and the todo tool called bare returns the plan for it. It
says nothing about services, so after a compaction an agent forgets
the dev server or watcher it started and starts another, or stops
looking at one that is still failing. Reported by the user 2026-09-05.

## Requirements

- The handover template gets a services section beside the todo list:
  each running service by name, command and purpose, or "(none)".
- The summarizing turn is told to look them up the way it looks up the
  todos: `service status` with no name lists them. If the compaction
  turn cannot call the service tool, the running services are
  injected into the compaction prompt instead, from the session's
  service manager.
- Nothing changes for sessions with no services: the section reads
  "(none)" and costs a line.

## Acceptance Criteria

- A compaction test with a running service produces a summary whose
  services section names it; with none, the section says so.
