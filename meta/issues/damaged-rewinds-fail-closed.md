# Damaged rewinds fail closed

## Summary

Full and incremental replay apply rewind markers with `Vec::truncate(to)`. An out-of-range `to` is a no-op, so a syntactically valid damaged marker disappears while the history it was meant to abandon remains active. Replay silently resumes with the wrong conversation.

## Requirements

- Validate each rewind target against the current folded stream before applying it.
- Reject invalid markers consistently in full replay and incremental tailing.
- Preserve replay equivalence between the two paths.

## Acceptance Criteria

- Tests cover out-of-range markers in store load, initial tail replay, and appended tail consumption.
- Invalid markers return a line-numbered corruption error rather than retaining the abandoned history.
- Valid single and nested rewinds continue to fold identically in every replay path.

## Notes

- Source: `crates/ilar/src/session/store.rs:945-965`, `crates/ilar/src/session/tail.rs:148-179`.
- Follow-up to the completed rewind work.
