# Group handovers stay out of personal memory

## Summary

Group seats cannot use personal memory tools, but their compaction handovers are written to shared daily memory. The private weekly job reads those notes and can promote their content.

## Requirements

- Keep group compaction summaries out of the personal memory store.
- Apply the same rule to normal and aborted turns and explicit compaction paths.
- Preserve private-chat handovers and the rule that weekly review does not target a group.

## Acceptance Criteria

- A group compaction leaves no summary in personal daily memory.
- A private compaction still writes its handover when memory is enabled.
- Regression tests cover aborted and explicit compaction paths where applicable.

## Notes

- Found in the [memory review](../memory-review-2026-11-07.md).
- `crates/ilar-gateway/src/gateway.rs:772-782,1793-1810`; `crates/ilar-gateway/src/weekly.rs:65-78`.
- The source confirms group-content retention when the write succeeds. Later promotion is possible, not guaranteed. This is not evidence of private-to-group disclosure.
