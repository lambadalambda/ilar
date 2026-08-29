# Parked steers survive the process

## Summary

`task_message` promises a parked steer "is not lost", but
`ChildSteers.pending` is process memory — quit or crash discards it
silently. Completions earned a durable outbox; steers did not.
Either soften the promise or give pending steers a disk copy
(adjacent to, not the same as, the parked mailbox-tool idea).

Size: M. Source: sweep 2026-08-29, subagent.
