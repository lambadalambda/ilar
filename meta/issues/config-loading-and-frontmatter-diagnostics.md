# Config loading and frontmatter diagnostics

## Summary

Config layers replace whole sections, I/O errors are suppressed, and malformed or CRLF frontmatter silently removes agents and skills.

## Requirements

- Merge provider, compaction, and subagent fields individually.
- Ignore only not-found config files; report other I/O failures with paths.
- Normalize CRLF and require exact frontmatter delimiters.
- Report malformed agent and skill definitions instead of silently dropping them.
- Validate semantic ranges and align examples with supported fields.
- Discover project `.ilar/agents` and project skills as documented.
- Resolve config/state directory overrides through the injected Loader environment.
- Reject unknown provider auth and flavor values.

## Acceptance Criteria

- Project overrides preserve omitted user-level fields.
- Permission and UTF-8 errors are surfaced.
- CRLF definitions load and malformed definitions produce diagnostics.
- Example configuration parses successfully.
- Agent-frontmatter examples contain only supported fields and parse in tests.
