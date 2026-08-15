# Subagent safety and outcomes

## Summary

Parallel subagents can mutate the same checkout, resume unrelated sessions, outlive cancellation, leak event queues, and report aborted runs as successful.

## Requirements

- Validate resumed task ownership, agent type, and active-session conflicts.
- Share background-task cancellation across nesting depths and abort on Escape.
- Drain or drop unused event receivers and prune completed handles.
- Report only completed outcomes and assistant-role final text.

## Acceptance Criteria

- Invalid task IDs are rejected.
- Abort and max-iteration outcomes return errors.
- Root cancellation reaches nested background tasks.
- Tool-only children cannot return their prompt as a result.
