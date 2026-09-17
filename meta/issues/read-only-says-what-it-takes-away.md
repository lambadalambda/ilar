# Read-only says what it takes away

## Summary

`explore` is described to the delegating model as "Read-only
repository explorer and code reviewer for parallel inspection" and
tells itself "Inspect, analyze, and review without modifying the
workspace" (config/mod.rs:75-82). Both sentences say *do not write*.
What the agent actually gets is four tools — `read`, `glob`, `grep`,
`webfetch` (tools/mod.rs:1208) — with no shell, no `websearch`, no
`todo`, no `skill`. A model reading the description delegates work it
cannot possibly do.

It happened in full in a charachat session on 2026-09-16: an `explore`
child was asked to review a plan and told, in its own prompt, "you can
decode it with python3". It noticed the gap in its first minute — "I
have no bash/python in this session (read-only tools)" — and then
spent forty minutes and 1.8 MB of log trying to reach the same answer
with `grep` regexes, one of which used a backreference the engine
rejects. Nothing in its instructions told it that the right move was
to say so and stop.

The deeper question the incident raises: a review that cannot run the
tests is half a review, and `explore` is the agent named for reviewing.
No comparable tool reads "read-only" as "no shell":

- **Claude Code** denies the write tools and keeps the rest. Its
  built-in Explore has Bash; the documented `code-reviewer` example is
  `tools: Read, Grep, Glob, Bash` — a reviewer that runs `git diff`.
- **Codex** restricts *effects*, not tools: `sandbox_mode =
  read-only | workspace-write | danger-full-access`, orthogonal to an
  approval policy. Commands always run; in read-only they cannot write
  or reach the network, and the agent escalates by asking.
- **OpenCode** has per-tool permissions of `allow | ask | deny`,
  overridable per agent, with glob rules for bash (`"git *": allow`,
  `"rm *": deny`). Its Plan agent keeps bash at `ask`. Its read-only
  subagents (Explore, Scout) do drop the shell — but General is right
  there for anything that has to run.

## Requirements

- The description a delegating model reads names the toolset, not the
  intent: no shell, so no tests, no builds, no git, no scripts.
- The agent's own prompt tells it to refuse work its tools cannot
  reach and say which tool it lacked, rather than improvise around it.
- Decide whether `read_only` should keep meaning "this list of four
  tools" or come to mean "the whole toolset minus what writes", and
  say which in docs/agents-and-skills.md either way.

## Notes

The third requirement is the one with a real trade-off, and it is not
the same trade-off the other tools face. A read-only agent in ilar
takes a `WorkspaceAccess::ReadOnly` lease, which is what lets several
of them run at once without serializing; hand them a shell and four
parallel reviewers run four `cargo test`s over one target directory.
Claude Code and Codex have no workspace lease to protect. So the
honest options are a shell-less `explore` that stays parallel and
says so, or a serialized `review` agent that may run things and is
told not to edit — possibly both.

Raised by the user, 2026-09-17: "agents seem to think that 'explore'
means 'don't write code and stuff', not 'you literally can't run
anything'."

Size: S for the wording, M for the decision. Source: session forensics
2026-09-17.
