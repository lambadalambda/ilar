# Esc spares detached children

## Summary

A background child's token is a child of the turn's token
(subagent.rs:1084-1086); Esc cancels that token (main.rs:4326-4331),
an aborted turn does not pause notifications (schedule.rs:190-193),
and a notification turn starts as soon as the keyboard is idle
(decide.rs:496). The user sees "aborting current operation…", the
`· bg` rows vanish, one "task ▸ X was cancelled." row per child, and
the status flips to `thinking`: a fresh turn nobody asked for. Ctrl-Q's
cancel-all pauses notifications for exactly this reason
(main.rs:3652-3663); Esc does not. Children from an earlier turn and
`/command` subtasks survive, so the rule is uneven. By design per
background-agents.md ("abort of parent kills children") but
docs/interface.md says Esc "never touches" standing state and
docs/agents-and-skills.md says a detached task leaves "the parent
free to keep working".

## Requirements

- Either spare detached children on Esc, or pause notifications on
  abort as Ctrl-Q does and say so in both docs.

Size: S. Source: UX sweep 2026-09-15, agents.
