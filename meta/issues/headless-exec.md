# ilar exec: run a turn without a terminal

## Summary

`ilar` only exists as a TUI. There is no way to run a prompt from a
script, a cron job, a git hook, or another program. `ilar exec "…"`
runs one turn headlessly and prints the answer.

It is also the first step of [a web frontend](web-frontend.md): a
second front end can only exist if the core is drivable without a
terminal, and the honest way to find out what is secretly coupled to
the TUI is to build the smallest possible second driver.

## Requirements

- `ilar exec [options] [prompt]` runs one turn and exits. With no
  prompt argument, the prompt is read from stdin, so
  `echo … | ilar exec` works.
- Options mirror the TUI's where they mean the same thing: `--model`,
  `--agent`, `--session <id>`, `--continue`.
- Default output is the assistant's final text on stdout and nothing
  else, so `ilar exec "…" > answer.md` is useful. Progress — tool
  calls, retries — goes to stderr.
- `--json` emits the loop's events as NDJSON on stdout for machine
  consumption, and nothing on stdout that is not an event.
- Exit code: 0 when the turn completed, non-zero when it failed, was
  aborted, or hit the iteration limit. Ctrl-C cancels the turn and
  exits non-zero.
- No terminal is required: no raw mode, no alternate screen, works
  under a pipe with no TTY.
- The `question` tool cannot block forever with nobody to answer: in
  exec mode a question returns an error telling the model to proceed
  on its own judgement.
- Services and background subagents do not outlive the process. Both
  are stopped on exit, and anything still running is named on stderr
  rather than silently killed.
- The session is a real session: resumable, checkpointed, and visible
  in the TUI's picker afterwards.

## Acceptance Criteria

- Tests drive `exec` end to end against a mock provider: text answer
  on stdout, tool progress on stderr, `--json` event stream,
  non-zero exit on failure, prompt from stdin.
- A test pins that a question asked in exec mode fails the tool call
  instead of hanging.
- The full suite passes, and the TUI still starts.

## Outcome

Landed in two commits: the shared runtime, then the driver. The
refactor the issue expected was real — `ilar::runtime` now owns agent,
model and reasoning resolution, the system prompt, session creation
and the tool wiring, in two phases so `--print-prompt` can answer
without creating a session. main.rs lost 312 lines.

Two headless-only decisions came out of it. Whether the `question`
tool is attached is now the caller's choice, because a driver with
nobody to answer would otherwise hang the turn on a channel nobody
reads; exec declines it. And exec stops background tasks and services
at exit, naming anything still running on stderr.

The `--json` projection is hand-written rather than derived: the wire
format should not change every time `LoopEvent` grows a variant. It is
also the first draft of what a web frontend would consume.

Verified against the real binary: answer on stdout with nothing else,
progress on stderr, NDJSON on stdout under `--json`, prompt from
stdin, `--continue` appending a second checkpointed turn to the same
session, and non-zero exit with a clean stdout when the provider
rejects the call.

## Notes

- Expect refactoring: the per-session bootstrap (agent and model
  resolution, skills, system prompt, spawner, registry, services,
  todos, tool context) is ~180 lines inside the TUI's main loop. Both
  drivers must share it rather than grow two copies — the point of
  this issue is to find where that seam belongs.

## Milestone

11 — Beyond the terminal
