# MCP via skill (no built-in MCP client)

## Summary

Decision: ilar will not grow a core MCP client. Integrations with MCP
servers happen through a skill that drives an external MCP-capable CLI,
consistent with the "skills over features" principle.

## Requirements

- A built-in skill (like the existing worktree-isolation skill) that
  teaches the agent to: discover configured servers from a user-provided
  config file, list a server's tools, call a tool with JSON arguments, and
  handle stdio vs HTTP servers — all via an external CLI the user installs
  (document at least one concrete choice and its install command).
- The skill documents security posture: MCP servers run outside ilar's
  process with whatever access the sandbox grants; no credential handling
  in ilar itself.
- README: short section pointing at the skill and stating the no-core-MCP
  decision.

## Acceptance Criteria

- Skill file ships in the built-in skill set and loads via the skill tool.
- Following the skill's instructions end-to-end against one real MCP
  server (manual verification) works.

## Notes

- Candidate CLIs evolve quickly; the skill should teach the pattern and
  name a default rather than hardcoding flags likely to rot.

## Resolution

Shipped as the built-in `mcp-via-cli` skill (mcptools CLI; stdio + HTTP,
config discovery, session caveats). Command syntax was verified against
upstream mcptools documentation; a live end-to-end run against a real
server was not possible in the sandboxed environment (package installs
blocked) and remains a manual follow-up.
