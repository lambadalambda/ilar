# Deterministic test harness

## Summary

Mock providers repeat their last script and provider tests can modify checked-in fixtures, hiding extra calls and making tests impure.

## Requirements

- Make mock script exhaustion a failure by default.
- Provide an explicit opt-in repeating mode for loop-guard tests.
- Never modify repository fixtures during tests.

## Acceptance Criteria

- Unexpected provider calls fail the invoking test.
- Existing intentional-repeat tests opt in explicitly.
- Tests pass in a read-only checkout with no worktree changes.
