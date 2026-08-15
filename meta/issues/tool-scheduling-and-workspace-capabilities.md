# Tool scheduling and workspace capabilities

## Summary

`ToolKind::ReadOnly` conflates concurrency safety with actual side effects, allowing task tools that can edit the workspace to overlap.

## Requirements

- Represent concurrency safety separately from workspace mutation capability.
- Make todo updates deterministic in tool-call order.
- Serialize mutable child tasks that share a checkout.
- Track explicit workspace identity/cwd and validated isolation metadata.
- Permit concurrent child work only when capabilities guarantee read-only access or a distinct validated worktree; otherwise serialize it.

## Acceptance Criteria

- Two mutable child tasks cannot share a checkout concurrently.
- Read-only or explicitly isolated children may overlap.
- Todo scheduling no longer relies on a misleading read-only classification.
- Executor barrier tests cover the new metadata.
