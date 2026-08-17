# Fix subagent launch regression

## Summary

Foreground task calls repeatedly fail in normal TUI use instead of starting a subagent, including retries that provide a validated isolated worktree.

## Requirements

- Recover and identify the concrete task-tool failure from the affected session.
- Fix the root cause without weakening task ownership, active-session, workspace, or cancellation safety.
- Keep nonblank invalid resume IDs rejected with actionable errors.

## Acceptance Criteria

- A normal foreground task can launch and return a subagent result in the reported repository scenario.
- A task using a validated sibling worktree can launch without repeated retries.
- Relevant regression tests and all workspace checks pass.
