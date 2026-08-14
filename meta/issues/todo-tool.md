# Todo tool

## Summary

Session-scoped todo list tool (TaskCreate/TaskUpdate/TaskList style or
single todowrite — decide during impl) rendered in TUI status area.

## Requirements

- Todo state persisted in session events.
- Tool(s) marked ReadOnly (safe to call concurrently).
- TUI: active todo list visible (collapsible pane or status block).
- Mutating = no; concurrency-safe = yes.

## Acceptance Criteria

- Tool round-trip tests; TUI shows list updating live during a scripted
  mock session.

## Milestone

3 — Polish & extras
