# A dead group is left in peace

## Summary

`ServiceManager` keeps a pgid after the shell is reaped (for
daemonized grandchildren), then `stop_all`/`Drop` SIGKILL the group
unconditionally (service.rs:53-59, process.rs:108-116). Once every
member has exited the pgid is reusable — a long-lived session can
killpg an unrelated process group, violating process.rs's own
SAFETY comment.

## Fix

Probe with `killpg(pid, 0)` and/or record group start-time; clear
`group` once a liveness probe says it is empty.

Size: S-M. Source: sweep 2026-08-29, store/tools.

## Outcome

`process_group_signalable` (killpg with signal 0) gates every kill, and
`ServiceEntry::refresh` drops a group id that no longer answers — so
the window between "the group emptied" and "somebody else got the id"
closes on the next status read rather than at session end. `EPERM`
counts as not-ours, which is what the unconditional kill achieved
anyway; a daemonized service still dies with its session, because a
live grandchild keeps the group answering.
