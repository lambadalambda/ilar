# Memory off stops review writes

## Summary

With `gateway.memory.enabled = false` and idle review still enabled, the chat has no memory tools but a review plan can write directly to the memory store when approval is off (the default). With approval on, the staged plan can still be applied through `/approve` without a memory-enabled check.

## Requirements

- Prevent idle-review core and archive edits when gateway memory is disabled.
- Keep skill review independent if it is enabled.
- Define and test how disabled memory interacts with staged plans and weekly memory work.

## Acceptance Criteria

- A qualifying review that returns memory edits does not create or change memory while the switch is off.
- Enabled memory still accepts valid review edits.
- Tests cover the chosen staged-plan and weekly behavior.

## Notes

- Found in the [memory review](../memory-review-2026-11-07.md).
- Runtime gate: `crates/ilar-gateway/src/driver.rs:934-949`.
- Review scheduling and direct application: `crates/ilar-gateway/src/gateway.rs:1689-1693,1765-1789`.
- This is an off-switch bypass for memory writes, not a requirement to disable all skill review.
