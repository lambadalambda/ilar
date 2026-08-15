# Bounded event channels

## Summary

Loop, notification, and subagent event channels are unbounded and can grow without limit when consumers stall.

## Requirements

- Bound channels that carry high-volume deltas or queued notifications.
- Coalesce adjacent text and thinking deltas where lossless.
- Define producer behavior under backpressure and cancellation.

## Acceptance Criteria

- A stalled consumer has bounded memory use.
- Stream ordering and terminal events remain lossless.
- Cancellation unblocks producers waiting on capacity.
