# Bound project context discovery

## Summary

Load project instructions only from the user configuration directory and the
exact working directory instead of searching arbitrary parent directories.

## Requirements

- Read `AGENTS.md`, falling back to `CLAUDE.md`, from the user configuration
  directory.
- Read `AGENTS.md`, falling back to `CLAUDE.md`, from the exact working
  directory.
- Combine both files when present, with working-directory instructions after
  user instructions.
- Do not discover context files from other ancestor directories.
- Document context and skill discovery locations in the README.

## Acceptance Criteria

- User and working-directory context can both appear in the system prompt.
- `AGENTS.md` wins over `CLAUDE.md` within each location.
- Parent-directory context outside the exact working directory is ignored.
- `ILAR_CONFIG_DIR` controls the user context and skill locations.
- Workspace tests, formatting, and clippy pass.

## Notes

- `system_prompt_for` now combines only the resolved user config directory and
  exact runtime working directory, in that order.
- Context reads fall back to `CLAUDE.md` only when `AGENTS.md` is absent;
  permission and UTF-8 errors retain the affected path and stop the turn.
- The resolved user config directory is propagated through direct, nested,
  isolated, and notification-routed subagent runtimes.
- README configuration documentation now covers all TOML fields, environment
  variables, context files, custom agents, and skills.
- Independent review found no remaining correctness or documentation findings.
