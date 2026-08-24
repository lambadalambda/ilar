# Let the agent revisit its subagents

## Summary

Subagent sessions are never destroyed — they are ordinary sessions on
disk, and the task tool already takes a `task_id` to resume one with
its full context (guarded on agent, parent and workspace). But nothing
ever tells the model an id: a foreground task returns only the child's
final text, a background task returns a fixed "started" string, and
the completion notification carries description and result only. The
schema meanwhile says "never invent a value". Resume is implemented,
tested, and unreachable — every subagent is one-shot in practice, so a
follow-up means spawning a fresh agent and re-explaining everything.

Second half of the same gap: the agent has no way to see which
subagents exist, which are still running, or what they last said.

## Requirements

- Every task result names the session that produced it, in text the
  model reads: foreground results, the background "started" string,
  and the completion notification. The tool description tells the
  model it may resume with that id.
- A `tasks` tool lists the current session's child tasks: id, agent,
  model, whether it is running now, how long ago it last spoke, its
  opening prompt, and a snippet of its last reply. Read-only,
  concurrency-safe, no arguments.
- Children of the invoking session only — a session cannot enumerate
  or resume another session's subagents (the resume guard already
  refuses; the listing must not leak them either).
- Bounded output: a snippet is a snippet, and the listing caps how
  many children it reports, saying so when it truncates.

## Acceptance Criteria

- Tests pin the id appearing in all three result paths, and a resume
  round-trip driven by the id the tool actually returned.
- Tests pin the listing: children only, running vs finished, snippet
  bounded, cap reported.
- The full suite passes.

## Notes

- Resume grows a child's context across follow-ups where one-shot
  tasks bound it. That is the point (a warm context is cheaper than
  re-explaining), but the tool description should steer the model to
  resume for follow-ups on the same scope, not to keep a general
  conversation going.

## Milestone

10 — Everyday polish
