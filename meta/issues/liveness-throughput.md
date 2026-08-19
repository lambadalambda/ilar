# Throughput in the liveness display

## Summary

Bytes total answers "is data arriving"; a rate answers "how fast".

## Requirements

- Track a windowed transfer rate (>=1s windows) during streaming.
- Display as `· 12.3 KiB/s` after the byte total in status and activity
  rows; hidden while stalled or before the first window.

## Acceptance Criteria

- Unit tests for the window math and formatting.
