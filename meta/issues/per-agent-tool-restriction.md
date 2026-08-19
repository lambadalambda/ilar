# Per-agent tool restriction

## Summary

Custom agent frontmatter supports `read_only` but no finer-grained tool
selection. Agents should be able to declare an allowlist.

## Requirements

- Frontmatter `tools:` — a list of tool names the agent may use; omitted
  means the current default set. Combines with `read_only` (intersection).
- Unknown tool names in the list are a load-time error surfaced like other
  agent-definition errors, not silently ignored.
- The task tool's agent-type description mentions restricted toolsets so
  the parent model can pick appropriately.
- This remains coordination, not a security boundary — README safety
  wording stays accurate.

## Acceptance Criteria

- Unit tests: allowlist filters the registry; intersection with read_only;
  unknown-name error.
- A restricted agent cannot invoke an excluded tool (tool error, not
  panic).

## Notes

- Keep the existing enforced read-only registry mechanism as the base.
