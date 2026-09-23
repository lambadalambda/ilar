# Background by default: the loose ends

## Summary

Tasks detach by default since `9813e02`, and a held checkout refuses
mutating calls since `a330757`. The sweep of 2026-09-23 found the
places that still assume the old world.

## Requirements

Bugs:
- `ilar exec` runs one turn and cancels background tasks at exit
  (crates/ilar-tui/src/main.rs `run_exec`, ~1265-1283), so any
  headless run that delegates loses the work. Wait for the
  notifications and run their follow-up turns before exiting (or run
  tasks in the foreground under exec). docs/sessions.md:262 follows.
- A background task's transcript row is ✓ once its started text
  returns, and stays ✓ when the child fails, stalls or is cancelled
  (transcript.rs `finish_tool_row` ~1623, `apply_subagent_activity`
  ~1730). Take the row's final state from the child's `TurnDone` or
  the completion notification.
- The stall watchdog's abort (main.rs ~3614) cancels this turn's
  detached tasks like Esc but neither pauses notifications nor says
  so; their "was cancelled" results start a turn against the provider
  that just went silent. Share the Esc arm's logic.
- `service status` / `service logs` are refused while a detached job
  holds the checkout: every `service` action is `Mutating`
  (service.rs:269). Only start/stop mutate.
- The held-checkout refusal sends the model to `tasks`, which does not
  list background bash jobs (session_id None). Name the holder in the
  refusal instead; `bash`'s `run_in_background` text says the job holds
  the checkout and must be the only call in its response.

Confusing:
- The refusal shows in the TUI as a red × failure in text written for
  the model. Give refusals their own look and name the holder; mark
  the holder's agents-panel row `· holds the checkout`.
- `· bg` is now on almost every panel row (sidebar.rs:232); mark the
  rare waited-on task instead. docs/agents-and-skills.md and
  docs/interface.md follow.
- Stale texts: "start one with the task tool's background flag"
  (subagent.rs ~1843); "its lease only delays your commit" (~3608,
  a commit is refused); "edit, write and bash" where service and sudo
  are refused too (~1631, ~3608, ~3694).
- The compaction template has `## Services` but no place for tasks and
  jobs still running.

## Acceptance Criteria

- Tests: exec keeps a delegated task's result; a failed background
  task's row ends failed; service status runs behind a held checkout.
- Full gate green.

## Notes

- Source: UX sweep 2026-09-23 (TUI, model words, docs passes). Size: M.

## Done (2026-09-23)

All of the above, in `ed58efc`..`18e0f21`: `exec` carries completions
as follow-up turns through `ilar::delivery::deliver_step`, now shared
with the gateway, and exits 130 when stopped while it waits; a task
row takes its child's ending; the watchdog pauses like Esc; `service`
status/logs/stop take no lease; a refusal names its holder through
`WorkspaceScheduler::hold_as`; the texts say "edit, write, bash,
service start and sudo" and the compaction template has `## Running`.

Left: the refusal still looks like a failure (red ×) in the TUI, the
panel does not mark the holder's row, `· bg` is still on every row,
and `## Running` gets no live list the way `## Services` does. The
queue/held loop is in two drivers (exec, gateway); one core driver
would end that.
