# Streaming tool progress

## Summary

Long streamed tool arguments need visible activity so users can distinguish active provider output from a stalled request and from filesystem execution.

## Requirements

- Show a monotonically increasing received-byte count while write arguments stream.
- Distinguish receiving arguments, waiting for the provider, and executing the write.
- Coalesce or throttle progress updates so UI visibility does not backpressure provider streaming.
- Keep progress transient and out of persisted session data.

## Acceptance Criteria

- A large streamed write visibly advances before arguments complete.
- A quiet provider stream shows how long it has been since the last argument data.
- Completed arguments transition the running row from receiving to writing.
- Progress updates remain bounded under a high-volume stream.
