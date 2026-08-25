# Codebase health sweep

## Summary

Before adding more features, sweep the whole codebase (~68k lines of
Rust across `ilar` and `ilar-tui`) for bad architecture, refactoring
and DRY opportunities, and bugs. The output is a triaged list of
findings — clear bugs and cheap wins as sub-issues, judgment calls
surfaced to the user.

## Requirements

- Cover both crates: core (agent loop, providers, session, tools,
  config) and TUI (app state, event loop, rendering, modals, input).
- Findings must cite file and line and describe a concrete failure or
  cost — no style nits, no speculative rewrites.
- Verify each reported finding against the actual code before
  accepting it.

## Acceptance Criteria

- A written, triaged findings report; confirmed problems filed as
  sub-issues in the tracker.

## Milestone

11 — Beyond the terminal

## Outcome

Six parallel reviewers covered both crates (app/event loop,
rendering, modals/input, agent core, providers/config/auth,
session/tools); every high-severity claim and ten spot-checked
findings were verified against the code before acceptance. Result:
27 sub-issues under Milestone 12 — 17 bugs (5 high: zai cache-usage
accounting, non-retryable 529, retry-now stranding the pending
manager, turn errors leaving subagent spinners, restored nested
thought id collisions), 9 refactors (the nine-picker skeleton, the
app.rs split, the shared SSE mapper core, …), and one batched
cleanup list. The reviewers also explicitly cleared the
highest-risk ground: atomic-file crash durability, store stamp
discipline, SSRF blocking, cache-prefix determinism, SSE framing,
auth refresh races, and the TUI's wrapping/width math.
