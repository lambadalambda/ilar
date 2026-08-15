# Secure atomic OAuth store

## Summary

OAuth credentials are written with default permissions, replaced non-atomically, and refreshed without synchronization.

## Requirements

- Persist credentials atomically with owner-only permissions.
- Surface malformed or unreadable stores separately from missing stores.
- Serialize refresh-token rotation and recheck state after acquiring the lock.
- Bound callback reads and accept connections until a valid callback or overall deadline.
- Parse percent-encoded callback parameters and OAuth errors correctly.

## Acceptance Criteria

- Token files are mode 0600 on Unix and survive interrupted replacement.
- Concurrent 401s perform one effective refresh.
- Slow or spurious callback connections cannot hang login.
- Callback parsing handles encoded values and denial responses.
