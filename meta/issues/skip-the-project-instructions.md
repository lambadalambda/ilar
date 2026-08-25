# Skip the project instructions

## Summary

A project's `AGENTS.md`/`CLAUDE.md` is unauthenticated third-party
input: often a year stale, occasionally actively hostile. The user
wants to keep the rest of the prompt chain (base prompt + user
context from the config dir) but skip the working-directory file
for specific launches, without deleting or editing the project's
file.

## Requirements

- A CLI flag (suggest `--no-project-instructions`) that skips the
  "Working directory context" entry in `system_prompt_for`
  (config/agents_md.rs); user-config context and the base prompt
  are unaffected.
- A config default (`general.project_instructions = true`) so a
  paranoid user can flip the default and opt in per launch instead;
  the flag always wins over the config.
- When skipped while a file exists, the TUI shows a startup notice
  ("project AGENTS.md present but skipped") so the state is
  visible — silently ignoring instructions invites confusion.
- Decide and document resume semantics: the system prompt is
  rebuilt at session start — a resumed session should honor the
  *current* launch's flag (the point is escaping a hostile file;
  resuming must not smuggle it back in). Add a test pinning that.

## Acceptance Criteria

- Flag on: prompt contains user context but no working-directory
  section even though the file exists; startup notice shown.
- Flag off (default): behavior unchanged.
- docs/configuration.md's "Project instructions" section documents
  both knobs and the security rationale.

## Milestone

12 — Health sweep

## Outcome

`system_prompt_for` returns a `SystemPrompt` whose skip branch never
even opens the project file (an `is_file` probe only). Both flags
(`--no-project-instructions` / `--project-instructions`,
conflicting) on `ilar` and `ilar exec`, over a user-scoped
`general.project_instructions` default — user-scoped deliberately,
so an untrusted project's ilar.toml cannot vote itself trusted (a
project declaring it gets the same startup warning as theme). The
startup notice names the real file and the real cause. Resume
honors the current launch (pinned by a three-way resolve test), the
override applies on every rebuild (picker switches and forks
included), and subagents inherit the refusal — pinned by
`a_refused_project_file_never_reaches_a_subagent`. Docs are honest
that the escape is prompt-level: the model can still `read` the
file. (The spawner's Include default is recorded in
sweep-deferred-decisions.)
