# A job death does not invent a task

## Summary

The abnormal-death guard's message tells the model to "resume it
with the task tool" and wears the task-notification envelope — but
`spawn_background_tool` jobs (subagent.rs:1520-1525) have no child
session and no task id. A panicked shell job invites the model to
invent one, in the exact way the schemas warn against.

## Fix

Parameterize the guard's drop message: tool-notification envelope
and no resume advice for jobs.

Size: S. Source: sweep 2026-08-29, subagent.

## Outcome

The guard carries what it is standing over: a task keeps the
`<task-notification>` envelope and its resume advice, a job gets the
`<tool-notification>` its siblings wear, names its job id, and is
offered nothing to resume.
