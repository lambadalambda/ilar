# Terminal stores get consolidated

## Summary

The gateway's store gets a weekly consolidation pass; terminal stores
never do. Claude Code's "dream" merges duplicates, deletes contradicted
facts and prunes the index after 24 h and 5 sessions.

## Requirements

- Decide the trigger (time and session count, like Claude Code, or on
  demand) and reuse the gateway's weekly prompt where it fits.

## Acceptance Criteria

- A design note in this file before code.
