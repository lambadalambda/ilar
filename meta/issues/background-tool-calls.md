# Background tool calls

## Summary

Long-running tools should be launchable without blocking the active turn, then
deliver their result back through the same notification lifecycle as background
agents.

## Requirements

- Allow eligible tool calls to opt into background execution.
- Return a stable task ID immediately and deliver the completed tool result as
  a queued synthetic notification.
- Apply a configurable default timeout with a per-call override.
- Preserve normal tool validation, workspace scheduling, output bounds, and
  cancellation behavior.
- Serialize workspace mutations without guaranteeing that a queued background
  job runs before later foreground work.
- Terminate the Bash process group on completion, timeout, or cancellation;
  commands must not escape containment with `setsid` or equivalent daemonizing.
- Distinguish successful, failed, timed-out, and cancelled outcomes.
- Prevent detached work and undelivered results from leaking after shutdown.

## Acceptance Criteria

- A long-running background tool does not block the initiating turn.
- Completion is delivered exactly once and can trigger the next parent turn.
- Default and per-call timeouts terminate the underlying tool work.
- Escape or root cancellation stops active background tools.
- Foreground behavior remains unchanged for ordinary tool calls.
