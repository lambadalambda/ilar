# User-invoked commands

## Summary

Skills are model-invoked: they are listed in the system prompt with
trigger phrases, and `/name args` expands to a meta-prompt asking the
model to call the `skill` tool
(`crates/ilar-tui/src/main.rs:262 skill_invocation_prompt`). That costs a
round trip and leaves the decision to the model — right for "use this
when it applies", wrong for "do exactly this now".

Commands are the other half: a markdown file whose body *is* the prompt.
Invoking `/name args` substitutes the arguments and submits the result
directly. No tool call, no model discretion, never auto-invoked.

opencode's shape, which we should follow closely enough that existing
command files are easy to port:

```markdown
---
description: Address Greptile PR comments
agent: build
model: anthropic/claude-sonnet-4-6
---
Address Greptile feedback on the current pull request.

Command arguments: $ARGUMENTS
```

- The body below the frontmatter is the template and is required.
- `$ARGUMENTS` is everything typed after the command name; `$1`, `$2`, …
  pull positional arguments.
- Optional frontmatter: `description`, `agent`, `model`, `variant`,
  `subtask` (`packages/core/src/config/command.ts`).

ilar already has every concept those fields need: named agents, models
with reasoning variants, and subagents via the `task` tool.

## Requirements

- Discover commands beside skills, using the same two roots: the user
  config dir and the project checkout. Skills use `skills` and
  `.ilar/skills` (`crates/ilar/src/skill.rs:119-120`), so commands use
  `commands` and `.ilar/commands`.
- Invoking `/name args` submits the substituted body as the prompt.
  Support `$ARGUMENTS` and positional `$1`, `$2`, …; leave unmatched
  placeholders empty rather than literal.
- Commands never appear in the system prompt and are never model-
  invokable. That is the whole distinction from skills.
- Commands join the existing slash completion and the command palette
  alongside skills, and get the same close-match suggestion on a typo
  (`close_skill_matches`).
- Honour optional frontmatter where it maps onto something we have:
  `agent`, `model`, `variant`. `subtask` runs the command through the
  `task` tool instead of the main session.
- A name collision between a command and a skill is resolved
  deterministically and reported, rather than silently shadowing.

## Acceptance Criteria

- Loader tests: frontmatter parsed, body preserved verbatim, missing
  body rejected with a clear error, project commands override user
  commands of the same name.
- Substitution tests: `$ARGUMENTS` with empty, single and multi-word
  input; `$1`/`$2` with fewer arguments than placeholders; a body
  containing a literal `$` that is not a placeholder.
- A command with `agent`/`model`/`variant` runs under those, and without
  them inherits the session's.
- Invoking an unknown `/name` suggests near matches across both commands
  and skills.
- Existing skill invocation is unchanged.

## Outcome

Landed. All five commands in `~/.config/opencode/commands/` load and
substitute unchanged. `resolve_slash` in main.rs owns the precedence —
commands, then skills, then near-match suggestions — and is unit tested,
rather than the logic living inline in a key handler.

Not done, and the issue should not read as if it were:

- **`agent`, `model` and `variant` are parsed but not honoured.** They
  are carried on `Command` so wiring them is additive, but nothing reads
  them yet, and the two acceptance criteria about running under them are
  untested because there is nothing to test.
- **`subtask` is not parsed at all.**
- **A command shadowing a skill is deterministic but not announced.**
  Every surface agrees the command wins and the skill stops being listed,
  but nothing says so. Reporting it wants the same warning channel
  `SkillStore::list` lacks.

Decisions worth knowing: `goal` is reserved, since `/goal` is handled
before commands and a command by that name could never run. Names must
match what `/name` accepts, so a dotted filename fails at load rather
than appearing in completion and doing nothing. `$` followed by a digit
is always a placeholder, so `costs $5` with fewer arguments empties —
there is no escape hatch today.

## Notes

- **Frontmatter: accept both formats.** Decided. ilar's skills use TOML
  (`description = "..."`, `triggers = [...]`); opencode's commands use
  YAML (`description: ...`). The loader should take either, so
  `~/.config/opencode/commands/*.md` port by copying. Detect by probing:
  a `key: value` line is YAML, `key = value` is TOML. Put the parser
  somewhere both skills and commands can share it — see
  [Load Claude and opencode style skills](portable-skill-formats.md),
  which needs the same thing.
- The palette already carries skills as dynamic items, so commands
  should be a small addition there rather than a new surface.
- Sample commands to port and test against live in
  `~/.config/opencode/commands/`: greptile, work-from-pleroma,
  work-from-yodl, work-on-issue, yodl-daily-update. All five use
  `description` plus `$ARGUMENTS` and nothing more exotic, which
  suggests the optional frontmatter can land later if it complicates
  the first cut.

## Milestone

6 — Hardening
