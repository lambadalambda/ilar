# Background jobs show in the sidebar

## Summary

A background bash job is invisible until its notification lands, so
a long render reads as a hung session. The agents panel already
lists running tasks and deliveries; a job belongs there too.

## Requirements

- A running background job is a row in the agents panel — ⚙, its
  command, "job", elapsed — for as long as it runs; the `tasks` tool
  lists it the same way.
- Clicking it does nothing beyond closing a focus view: a job has no
  session of its own.

## Acceptance Criteria

- Tests: the running registry holds the job while it runs and not
  after; the panel renders a job row.
