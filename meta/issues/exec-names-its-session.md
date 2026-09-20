# exec names its session

## Summary

`ilar exec` never says which session it created: nothing on stderr
(main.rs:939-1030) and no `session` event in the JSON stream
(exec.rs:115-175), so `--session <id>` (main.rs:145) has nothing to
take from a previous run. `TurnOutcome::MaxIterations` exits 2 with
no stderr line (only `Err` writes `error:`), and progress rows are
bare `· read` × N with no argument summary.

## Requirements

- One `session <id>` line on stderr and a `session` JSON event at
  start; a `stopped: iteration cap` line on that exit; a short
  argument in each tool progress row.

Size: S. Source: UX sweep 2026-09-03 (overlays).

## Outcome (2026-09-20)

All four done, in `exec.rs`.

- **The session leads.** `session_line` prints `session <id>` on stderr,
  or `{"type":"session","id":"…"}` as the first stdout event under
  `--json`. It is emitted before `run_turn`, so a run killed halfway
  still told the script which session holds the work.
- **A turn that stopped short says why.** `outcome_line` prints
  `stopped: the step cap was reached before an answer — ilar exec
  --session <id> carries on from here`, and the same for an interrupt.
  Text mode only: under `--json` the outcome already rode `turn_done`.
  `Completed` says it with the answer, and an `Err` already had its own
  line in main.rs.
- **Tool rows carry their argument.** `ToolArguments` keeps the raw
  input from `ToolInputComplete` until the `ToolFinished` under the
  same id, which is where the tool's name is, then summarises through
  `summarize_tool_input` — bounded and redacted, since a progress row
  can end up in a log. `· read src/main.rs`, `✗ read nope.rs: no such
  file`. The entry is taken, not read, so a long turn does not carry
  every call it ever made.

`render_event` gained an `argument` parameter and stayed pure; the map
lives in `exec_turn`. docs/sessions.md says all of it.
