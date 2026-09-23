# The fixed prefix sheds weight

## Summary

Every request carries ~31 KB of tool schema. Some of it is dead or
said three times.

## Requirements

- Children at the depth limit get task, tasks and task_message
  (`agent_registry` → `with_subagents` with no depth check,
  subagent.rs ~932, tools/mod.rs ~1456) — ~6 KB per request, every
  call fails "nesting limit reached". With `max_depth = 1` that is
  every build child. Gate on `depth + 1 < max_depth`.
- The task tool says the background rule three times (description,
  `background` param, started text), the worktree recipe three times,
  "prefer read-only" three times. The sweep proposed a ~900-char
  description; the `prompt` param carries a sentence meant for the
  parent; task_message's `workspace` can be one line.
- `subagent_type` joins agent lines into "parallel.. Prefer" and
  lists review's tools twice.
- The question schema's three `oneOf` variants (question.rs:343-350)
  compact to one object (~1.9 KB → ~0.7 KB).

## Acceptance Criteria

- A test: a child at the depth limit has no task tools.
- `--print-prompt` byte count before/after recorded here.
- Full gate green.

## Notes

- Source: UX sweep 2026-09-23 (model words pass). Size: S-M.
