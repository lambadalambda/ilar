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

## Outcome (2026-09-05)

The handover template has a Services section between Plan and Next
Move, and the rules line names the service tool's bare `status` beside
the todo tool. The summarizing turn still calls no tools, so the live
list comes from the session's service manager: `ToolRegistry` keeps
the manager it installed the service tool with and answers
`running_services()` as `name · command` lines; both in-turn compaction
sites and the manual `/compact` pass it through `CompactionOptions`,
and the summarizer's instruction appends "Services running right now"
with the list. Tests: the registry lists a started service and forgets
a stopped one; a compaction with a running service carries it into the
instruction and one without claims no list.
