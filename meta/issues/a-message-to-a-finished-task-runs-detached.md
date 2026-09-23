# A message to a finished task runs detached

## Summary

`task_message` to a finished task resumes it with
`background: Some(false)`, so the parent's step holds for the whole
resumed turn: 785 of 1,375 `task_message` steps in the logs took 30 s
or more, median 43 s ([[a-slow-call-does-not-hold-the-step]]). Since
`9813e02` every task runs detached unless told otherwise; this resume
is the one path that still does not.

## Requirements

- `task_message` takes the same optional `background` as `task`,
  with the same default: omitted, a resume of a finished task runs
  detached, and its answer arrives as a completion notification.
- `false` keeps today's behaviour: the call returns the answer.
- The TUI's focus view keeps a foreground resume: the person is
  waiting for the answer in the notice, and a detached resume would
  wake the root model with it instead.
- The tool text says the new rule.

## Acceptance Criteria

- A test: a defaulted message to a finished task returns the started
  note at once, and the answer arrives as a notification.
- The existing resume tests pass with `background: false`.
- Full gate green.

## Notes

- Source: measurement, 2026-09-23. Size: S.

## Done (2026-09-23)

`task_message` takes the task tool's optional `background`, passed
straight to the resume, so omitted means detached and the started note
is the call's result. The TUI's focus view forces a foreground resume:
the person is the one waiting there. The two resume tests that read
the answer inline say `background: false`; a new one pins the default.
