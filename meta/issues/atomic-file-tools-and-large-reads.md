# Atomic file tools and large reads

## Summary

Read cannot window large files, while write and edit can truncate existing files before a failed replacement completes.

## Requirements

- Apply read offset and limit without loading or rejecting the full file first.
- Move blocking filesystem scans off async runtime workers where needed.
- Bound grep per-file, per-line, and total retained output; stop glob collection at its cap.
- Make long scans cancellable.
- Write and edit through same-directory temporary files and atomic rename.
- Preserve relevant file permissions.
- Distinguish empty files from offsets beyond EOF.

## Acceptance Criteria

- A small window can be read from a file larger than the global byte cap.
- Simulated replacement failures preserve original file content.
- Offset-past-end diagnostics are accurate.
