# Serialize shared memory writes

## Summary

Independent `MemoryStore` instances can share a directory but not their mutex. Concurrent successful core updates can overwrite each other. Terminal sessions and worktrees intentionally share the same project store.

## Requirements

- Serialize read-modify-write operations across instances and processes that name the same store.
- Keep atomic file replacement; do not treat it as a substitute for writer coordination.
- Cover core edits and competing amendments of an existing note without changing unrelated memory policy.

## Acceptance Criteria

- A regression test with independent stores retains every successful unique core addition.
- A cross-process test checks the shared terminal/worktree case.
- Existing amendment, forgetting and import tests remain green.

## Notes

- Found in the [memory review](../memory-review-2026-11-07.md).
- `crates/ilar/src/memory.rs:773-785,821-843,957-987`.
- An isolated diagnostic used two stores on one temporary directory, with 50 barrier-synchronized rounds. All 100 adds succeeded; only 50 lines remained.
- This issue records a confirmed problem. No runtime fix was made during the review.
