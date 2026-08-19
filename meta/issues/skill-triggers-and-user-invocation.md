# Skill triggers + user skill invocation

## Summary

Skill frontmatter `triggers` is parsed into `_triggers` and never used, and
skills can only be invoked by the model via the skill tool — the user
cannot fire one directly.

## Requirements

- Wire `triggers`: include each skill's name, description, and trigger
  hints in the system prompt's skill inventory so the model reliably
  invokes matching skills (prompt-level, no keyword scanner in core).
- User invocation: typing `/<skill-name> [args]` as the entire prompt
  submits a turn that instructs the model to invoke that skill with the
  given arguments; unknown names get an inline error listing close
  matches.
- `/` on an empty input opens a completion popup of available skills with
  their descriptions.
- Remove the `_triggers` dead field status: it is either used or the
  README stops calling it reserved.

## Acceptance Criteria

- Unit tests: trigger hints appear in the composed system prompt; slash
  parsing (name/args splitting, unknown-name error).
- A `/name` prompt visibly invokes the skill tool in the transcript.

## Notes

- Keep the popup implementation aligned with the existing picker widgets.
