# Index session replay

## Summary

Long append-only sessions replay all historical events, including compacted-away history, into memory on every load.

## Requirements

- Add a replay checkpoint or index after recovery semantics stabilize.
- Skip historical events that cannot affect active transcript state.
- Preserve auditability of the original JSONL log.

## Acceptance Criteria

- Loading a long compacted session scales with active history rather than the full log.
- Checkpoint corruption falls back safely to canonical JSONL replay.
- Session behavior remains identical with and without an index.
