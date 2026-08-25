# HTTP 529 should retry

## Summary

`retryable_status` (provider/transport.rs:30-41) lists
408/409/429/500/502/503/504 but not 529, the Anthropic-protocol
"overloaded_error" status. The body-based retryability heuristic only
applies to in-stream error events, never to non-2xx responses — so a
momentary overload on the z.ai Anthropic endpoint kills the turn
instead of retrying.

## Requirements

- Treat HTTP 529 as retryable in `retryable_status`.

## Acceptance Criteria

- A test asserting a 529 response yields `RetryableError`, not
  `Error`.

## Milestone

12 — Health sweep
