# Load Claude and opencode style skills

## Summary

ilar's skills are flat `skills/*.md` files with TOML frontmatter
(`description = "..."`, `triggers = [...]`), name taken from the file
stem (`crates/ilar/src/skill.rs:118-133`).

Claude Code and opencode both use a different shape: a directory per
skill, `skills/<name>/SKILL.md`, with YAML frontmatter carrying `name`,
`description` and extras like `allowed-tools`, `compatibility`,
`metadata`, `hidden`.

So an existing skill collection cannot be used with ilar at all — it
differs on both axes, format *and* layout. There are nine such skills in
`~/.config/opencode/skills/` today (agent-browser, claude-print,
eosrift-tunnel, nas-podman, obsidian-memory, repo-issues,
search-and-research-with-tavily, simulator-automation, tea-pleroma),
none of which load.

This follows the decision on
[User-invoked commands](user-invoked-commands.md) to accept both
frontmatter formats. The same parser serves both, so they should share
it.

## Requirements

- Accept YAML frontmatter alongside TOML, for skills and commands alike,
  through one shared parser. Detect by probing the first key line rather
  than by file location.
- Accept the directory layout `skills/<name>/SKILL.md` alongside flat
  `skills/<name>.md`. Directory name is the skill name; a `name:` field
  in frontmatter overrides it.
- Map the fields we understand and ignore the rest without failing:
  `description` maps directly, and Claude's convention of embedding cue
  phrases in the description means `triggers` stays optional.
- Unknown frontmatter keys are preserved-and-ignored, not an error. That
  is what makes a foreign skill directory usable unchanged.
- Precedence and override rules stay as they are: built-ins, then user
  dir, then project, later wins by name.

## Acceptance Criteria

- A skill in `skills/<name>/SKILL.md` with YAML frontmatter loads, and
  its name, description and body match the file.
- A skill with unknown keys (`allowed-tools`, `hidden`, `metadata`)
  loads without error and ignores them.
- Existing TOML flat-file skills keep working unchanged, including the
  two built-ins.
- A YAML `name:` that disagrees with the directory name wins, and the
  disagreement is not silently confusing.
- Test fixtures copied verbatim from `~/.config/opencode/skills/` load.

## Outcome

Landed. All nine skills in `~/.config/opencode/skills/` load unchanged,
with full-length descriptions; two of them are checked in verbatim under
`crates/ilar/tests/fixtures/skills/` so that stays true.

The YAML subset lives in `crates/ilar/src/config/frontmatter.rs` with its
own unit tests, and returns an `extras` map so a command's `model` or a
skill's `allowed-tools` survives the parse before anything honours it.

Review caught three silent wrong-value paths, all of which real Claude
Code frontmatter hits: a block scalar was truncated at the first blank
line, `|+` and `|2` style indicators made the description literally
`"|+"`, and a plain value wrapped across lines lost everything after the
first. All fixed and covered.

Two things deliberately left:

- **A `name:` that collides is silent.** Two directories declaring the
  same name collapse to one, and a foreign skill can shadow a built-in.
  Reporting it needs a warning channel `SkillStore::list` does not have —
  it returns `Vec<Skill>` and callers treat any `Err` as fatal.
- **One malformed file still fails the whole load**, which now also
  takes down the TUI at startup. That was already the policy and there
  is a test pinning it, but pointing ilar at a third-party collection
  makes it much likelier to bite. Worth deciding between fail-fast and
  skip-and-warn; both want the same warning channel as above.

`deny_unknown_fields` on the TOML side is gone, so a typo'd key in one of
our own skills now falls back to the filename instead of erroring. That
is the direct cost of the issue's own "unknown keys are
preserved-and-ignored" requirement, which foreign files need.

## Notes

- Deliberately one-way: read foreign formats, keep writing our own. No
  conversion tooling, no migration.
- `allowed-tools` is worth a second look later — it expresses a
  per-skill tool restriction, and ilar already has per-agent tool
  restriction. Out of scope here; loading the field without honouring it
  is the honest first step.
- Skill bodies referencing sibling files (`references/*.md`) work
  differently in a directory layout than a flat one. Loading the body is
  enough for now; relative-path resolution can wait for a real case.

## Milestone

6 — Hardening
