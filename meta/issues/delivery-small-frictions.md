# Delivery: small frictions

## Summary

Omnibus; tick items off here.

- A ✉ row has no visible exit on the resume path: `Propagate` is
  filed silently (schedule.rs:349); the "✉ … delivered to …" line
  prints only on the steer path (339-347). A nested `✉ … delivering
  · 12s` row vanishes wordlessly and minutes later "task ▸ nested: …
  completed." starts a root turn. docs/interface.md:210-214
  describes row-then-line as one sequence. Print a quiet line.
- A stalled child looks busy until it is killed: `stall_timeout` is
  hard-coded 600 s (subagent.rs:450; `with_stall_timeout` is never
  called), the row ticks, then "task ▸ X stalled: no progress for
  600s. It has been stopped." No doc mentions the watchdog;
  `subagents.background_tool_timeout_ms` is a different timer. A
  `quiet Ns` marker on the row and the timeout named in
  docs/configuration.md.
- A background mutable task waiting for the workspace shows as
  running (the wait notice is `None` for background,
  subagent.rs:1125-1127); docs/agents-and-skills.md's "(A background
  task has no row; it reports through its completion notification
  instead.)" is false as written, the task has a row and a "running
  in the background as build" line. `· waiting for the workspace` on
  the row; rewrite the sentence.
- `/command` subtasks surface model-facing errors: `agent: nope`
  gives "/name: unknown subagent_type "nope"; available: build,
  explore" (main.rs:2200-2204); docs/agents-and-skills.md "Commands"
  never mentions the `agent`, `model`, `variant`, `subtask`
  frontmatter keys command.rs:133-140 reads.

Size: S. Source: UX sweep 2026-09-15, agents.
