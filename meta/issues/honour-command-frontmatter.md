# Honour command frontmatter

## Summary

[User-invoked commands](user-invoked-commands.md) landed with `agent`,
`model` and `variant` parsed but ignored, and `subtask` not parsed at
all. The fields sit on `Command` doing nothing, and the closed issue
says so. This issue wires them up.

Semantics, following opencode closely enough that existing command
files behave as their authors expect:

- **`model` / `variant` (no subtask): a one-turn override.** The turn
  the command starts runs under that model; the session reverts when
  the turn ends. Persisted as two `ModelChange` events, so the session
  file honestly records which model produced which turn — replay and
  the transcript agree. A `variant` alone applies to the current model.
- **`agent`, or `subtask: true`: the command runs as a background
  subagent** via `SubagentSpawner::run_task`, with the expanded body as
  the prompt, `agent` as the subagent type (default `build`), and
  `model`/`variant` passed through the task tool's existing override
  path. Completion arrives as a notification, like every background
  task. This diverges from opencode (their subtask runs inline in the
  conversation) because background-plus-notification is ilar's native
  shape for exactly this.

## Requirements

- `subtask` parses to a bool on `Command`; absent means false.
- A command with `model`/`variant` starts its turn under them and the
  session reverts at turn end — including aborted and errored turns.
  The status line shows the override while it runs; the transcript
  records both switches.
- Validation at invocation, not load (a foreign file with an unknown
  model must still list): unknown model or invalid variant declines
  the send with a notice and restores the input, like an unknown
  `/name` does. A model with no configured provider declines the same
  way at spawn.
- An `agent`/`subtask` command spawns through `run_task` with
  `background: true`; the main session stays idle and the sidebar's
  background count picks it up. Unknown agent names decline with a
  notice listing the known ones.
- Every start path honours the overrides: interactive Enter, queued
  send, retry, and the pending-manager's retry.
- A command with none of these fields behaves exactly as today.

## Acceptance Criteria

- Loader test: `subtask: true` / `subtask = true` parse; anything else
  is false.
- `prepare_prompt`-level tests: a model-override command sets the
  override and returns the prompt; an unknown model declines with a
  notice and a restored input; a subtask command produces a task
  request and no main-session prompt.
- The revert state transition is pinned at the App level (the
  `run_app` persistence half is reading-verified, like every spawn
  effect — see [the harness issue](loop-schedule-harness.md)).
- All five commands in `~/.config/opencode/commands/` still load and
  run unchanged (none use these fields beyond `description`).

## Outcome

Landed as specified, with two review catches worth recording:

- **The root `ToolContext` carries no session id** — `run_turn` fills
  it per turn, and the UI subtask call bypasses `run_turn`. Unfixed,
  every subtask completion arrived as an unroutable notification that
  paused the pipeline and re-queued itself forever. The subtask block
  now sets the session id on a cloned context.
- **Orphan subagent activity is dropped, not buffered.** A UI-spawned
  subtask has no parent tool call id, so its activity can never attach
  to a Tool row; buffering it filled the 256-entry retry queue for the
  session's lifetime and evicted activity that could still attach.

Known rough edges, deliberate for the first cut: a failed revert
(persist error) strands the session on the override model with only a
notice; the status line reads "ready" for one frame during an
overridden turn's spin-up; an unknown agent name declines with the
known list but does not restore the input; and a subtask's transcript
presence is the start/finish system lines plus the background counter,
not a live activity row.

## Notes

- `adopt_model_selection` already does persist + App update + system
  line for the picker; the override and the revert reuse it rather
  than growing a second model-switch path.
- The override travels as App state (`pending_model_override`) set by
  `prepare_prompt` and applied in the spawn block, because the spawn
  block is where resolver and store live; the decision stays testable,
  the effect sits with its peers.
- UI-spawned subtasks have no parent tool call id; the subagent
  transcript row may not attach. The notification announces completion
  either way. Acceptable for the first cut; note it if it bites.

## Milestone

7 — Unscheduled
