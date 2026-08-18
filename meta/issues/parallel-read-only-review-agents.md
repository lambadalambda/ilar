# Parallel read-only review agents

## Summary

Repository review tasks are commonly delegated to several `build` agents, which request mutable workspace leases and therefore serialize. Long tool arguments can also consume the row and hide whether a bash call is queued or executing.

## Requirements

- Keep active tool state visible even when arguments are long.
- Explain and preserve safe serialization for genuinely mutable agents.
- Provide a configured read-only review/exploration agent that can run concurrently in the same checkout without destructive tools.
- Make task guidance steer repository inspection and review work to the read-only agent rather than mutable `build` agents.
- Preserve explicit `build` delegation for tasks that need edits or mutating tools.

## Acceptance Criteria

- Queued/executing state remains visible on narrow or argument-heavy tool rows.
- Several read-only review agents can acquire shared workspace access and run concurrently.
- The task tool clearly identifies the read-only agent as the appropriate choice for review and exploration.
- Read-only agents cannot access mutating tools, bash, or nested delegation.
- Full workspace checks and focused concurrency regressions pass.
