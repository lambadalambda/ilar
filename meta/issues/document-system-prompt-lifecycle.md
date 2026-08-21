# Document system prompt lifecycle

## Summary

Add focused documentation outside the README for how ilar assembles, reuses, and refreshes system prompts, including subagent and compaction behavior.

## Requirements

- Add a `docs/` document describing root-session prompt composition and recalculation timing.
- Explain that turns, retries, notifications, and compaction reuse the session runtime's prompt.
- Explain subagent prompt construction and the effect of changing `AGENTS.md`, agent definitions, or skills during a session.
- Document how compaction handovers appear in provider input and append-only session storage.
- Keep README coverage brief and link to the detailed document.

## Acceptance Criteria

- The detailed document is linked from README.
- Statements match current root-session, subagent, and transcript code paths.
- Markdown links and formatting are valid.

## Milestone

7 — Unscheduled
