# Write tool progress and stalls

## Summary

Write calls can appear to hang in the TUI, and the running tool row does not show the target path until the provider finishes streaming all arguments.

## Requirements

- Show the write target path as soon as it is available.
- Keep file writes from blocking the async runtime.
- Preserve atomic replacement and workspace scheduling semantics.

## Acceptance Criteria

- A running write row identifies its target file before execution completes.
- Large or slow writes do not stall unrelated async progress.
- Write success, failure, cancellation, and atomicity behavior remain covered.
