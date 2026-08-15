# Crash-safe session recovery

## Summary

Malformed JSONL tails and unmatched tool calls/results can make resumed sessions permanently unusable.

## Requirements

- Repair or truncate an unterminated malformed final record before appending.
- Truncate a malformed or invalid-UTF-8 final tail to the last valid newline.
- Reject middle corruption without mutating the log.
- Validate metadata, session identity, and tool-call/result pairing during replay.
- Persist synthetic error results for unanswered calls and reject orphan results.

## Acceptance Criteria

- Append after a torn non-newline tail preserves the new event across reloads.
- Crash-shaped unanswered calls resume with synthetic error results.
- Orphan results, duplicate IDs, duplicate metadata, and metadata/filename mismatches are rejected.
