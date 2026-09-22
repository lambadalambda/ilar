# Subagents run in the background by default

## Summary

A task's `background` follows its agent: read-only detaches, mutable
runs in the turn. The mutable case is the one that hurts. While the
parent is blocked inside a foreground `task` call, a message the
person sends is a steer, and a steer is read at the parent's next
step — which is after the task returns. So anything thought of
mid-flight waits for the whole subagent, and `task_message` cannot
reach a foreground task at all.

Codex and Claude Code both run subagents in the background only.
Codex's `spawn_agent` returns an id; `wait_agent` is a separate tool
its guidance says to call "very sparingly", and steering is
`send_input`. Claude Code's Agent tool has no foreground mode.

## Requirements

- `background` omitted means background, for every agent. An explicit
  `false` runs the task in the turn, as Codex's `wait_agent` does.
- A mutable background task without a workspace of its own still runs
  in the parent's checkout and serialises behind its write lease — the
  ordering guarantee stays — and the task's own result says the
  parent's edits wait until it reports.
- The capacity demotion stays: a defaulted background task that cannot
  detach runs in the turn and says so; an explicit `true` errors.
- The tool text, the `background` schema, and `docs/agents-and-skills.md`
  say the new rule. "Never poll a detached task" stays.

## Acceptance Criteria

- `a_mutable_task_defaults_to_the_foreground` becomes its opposite and
  passes; `an_explicit_foreground_beats_the_default` covers a mutable
  agent too.
- A test: a defaulted mutable task in the parent's checkout answers
  as a notification, and the started text names the held checkout.
- Full gate green.

## Notes

- Later, what Codex does: fork a worktree automatically for a mutable
  background task instead of asking the model to create one. Not this
  issue.
- Source: user request, 2026-09-22. Size: S.

## Done (2026-09-22)

`background` omitted is background, for every agent; `false` is the
caller saying it is blocked on the answer. A mutable task in the
parent's own checkout still serialises behind the write lease, and its
started text now says the parent's edits wait until it reports, so the
parent reads meanwhile rather than finding out from a tool row that
waits. The capacity demotion is unchanged in shape and reworded. The
tool description, the `background` schema and the agents doc say the
new rule; the `review` agent's paragraph no longer claims it runs in
the foreground "like build".

Forty-odd tests in the two task suites waited on results in the turn
and never said so; they pass `background: false` now, which is what
they meant. Two new tests pin the default for a mutable agent and the
held-checkout sentence.
