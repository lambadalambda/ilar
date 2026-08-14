# Config: TOML + markdown agent definitions + AGENTS.md injection

## Summary

Configuration loading: `~/.config/ilar/ilar.toml`, custom agents from
`~/.config/ilar/agents/*.md`, project context from AGENTS.md/CLAUDE.md
discovery.

## Requirements

- Parse TOML: default model, provider settings (base_url, api_key
  reference, flavor), compaction threshold, subagent caps.
- Env-var key lookup: `ILAR_OPENAI_API_KEY`, `ILAR_ZAI_API_KEY`.
- Project-local override: `ilar.toml` / `.ilar/ilar.toml` in cwd wins.
- Markdown agents: frontmatter (description, model, disabled) + body =
  system prompt. Merge with built-ins (build, plan later).
- Project context: nearest AGENTS.md or CLAUDE.md up the tree from cwd,
  content appended to system prompt.
- TDD: fixture config dirs (respect `ILAR_CONFIG_DIR` env for tests).

## Acceptance Criteria

- Tests: precedence (project > user > defaults), agent MD parse,
  AGENTS.md discovery from nested cwd.

## Notes

- No permission parsing — agent frontmatter may carry informational
  tool hints but nothing is enforced.

## Milestone

1 — Lean core
