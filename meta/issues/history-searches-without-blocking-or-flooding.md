# History searches without blocking or flooding

## Summary

The model-facing `history` tool synchronously reads and parses the complete session archive inside an async tool future. Speaker-only listings also return every matching entry: each row is bounded, but the aggregate result is not. Long sessions can stall a Tokio worker and then persist and resend an oversized tool result.

## Requirements

- Move full archive replay off the async runtime worker and make cancellation observable during the scan.
- Bound speaker listings by row count and aggregate bytes or characters.
- Report omitted rows explicitly and provide a way to page or narrow the result.

## Acceptance Criteria

- A blocking-pool test proves the runtime worker is not occupied by archive I/O.
- Speaker listing tests cover row and aggregate-output caps with a truthful omission marker.
- Query and event-context behavior remains addressable and bounded.

## Notes

- Source: `crates/ilar/src/tools/history.rs:125-180`, `crates/ilar/src/recall.rs:180-194`, `267-271`, `crates/ilar/src/session/store.rs:275-283`.
- Found by the current codebase review.
