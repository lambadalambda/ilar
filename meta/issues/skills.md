# Skills (markdown, incl. git-worktree-isolation skill)

## Summary

Skills: markdown files (frontmatter: description, triggers) in
`~/.config/ilar/skills/` and `.ilar/skills/`, surfaced to the model as a
skill tool/listing, body injected when invoked. Worktree isolation is a
skill, not core.

## Requirements

- Skill discovery (user + project), listing in system prompt (name +
  description only).
- `skill` tool: body injected into conversation on invocation (as tool
  result or synthetic message).
- Ship built-in example skill: `git-worktree-isolation` — instructions
  for spawning subagent work in `git worktree add`, mapping results back.
- Skills can bundle references: optional directory of extra files
  loaded on demand.

## Acceptance Criteria

- Unit tests: discovery, listing, injection.
- Manual: worktree-isolation skill drives a subagent that works in an
  isolated worktree end-to-end.

## Notes

- This replaces Claude Code's plugin/skill machinery with ~200 lines.
  Keep it dumb.

## Milestone

3 — Polish & extras
