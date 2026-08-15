# Atomic file replacement

## Summary

Credential and source-file updates need one tested same-directory replacement primitive rather than independent truncating implementations.

## Requirements

- Write through a unique same-directory temporary file and atomic rename.
- Preserve or explicitly set permissions before publication.
- Clean up temporary files on failure.
- Refuse symlink destinations, create temporary files exclusively with no-follow semantics, and verify the destination parent is unchanged before rename.
- Define durability as process-crash safety, with data and directory syncing where supported.
- Surface sync and cleanup errors without hiding the primary replacement failure.

## Acceptance Criteria

- Injected write and rename failures preserve the original file.
- Successful replacement publishes complete content only.
- Existing modes are preserved when requested and secret files can force 0600.
- Temporary files do not remain after handled failures.
