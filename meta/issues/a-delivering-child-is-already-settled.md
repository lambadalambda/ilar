# A delivering child is already settled

## Summary

Opening focus on a roster row marked `delivering` seeds the transcript as settled but initializes the focus footer as running. Delivery emits no child activity, so no `TurnDone` arrives to clear the footer and it can claim the child is running forever.

## Requirements

- Derive the focus view's initial `running` state from the same streaming predicate used for replay liveness.
- Keep genuinely running and resumed child turns live.

## Acceptance Criteria

- A regression test opens a delivering row and observes a settled focus view.
- Existing tests for focusing running and completed children still pass.

## Notes

- Source: `crates/ilar-tui/src/main.rs:2113-2169`, `crates/ilar-tui/src/app.rs:1301-1324`.
- Follow-up to the completed `the-focus-view-settles-what-it-saw-running` issue.
