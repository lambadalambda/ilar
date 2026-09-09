# The gateway loads only its own skills

## Summary

A gateway session lists the two built-in skills (worktree-isolation,
mcp-via-cli) and whatever `.ilar/skills` the process's working
directory holds, beside the home's own. The assistant should see only
what is in `<home>/skills/`.

## Requirements

- A runtime option for a home-only skill store: no built-ins, no
  project `.ilar/skills`; the gateway sets it. The terminal agent is
  unchanged.
- The `skill` tool reads the same store, so a name outside the home
  is unknown to it too.

## Acceptance Criteria

- Tests: the store lists only the home's skills under the option;
  a gateway chat's prompt lists no built-in.
