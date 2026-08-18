# Task delegation guidance

## Summary

The task tool's `background` option is easy to interpret as ordinary parallelism even though it creates a deferred completion and separate follow-up turn, allowing the parent to duplicate delegated work and answer before the result arrives.

## Requirements

- Explain that delegation transfers ownership of a clearly bounded scope.
- Direct current-answer dependencies to foreground tasks, including parallel sibling calls.
- Reserve background tasks for intentionally deferred follow-up work.
- Tell parents to continue only disjoint work after a background launch.
- Avoid fuzzy runtime overlap blocking that would reject legitimate independent reviews.

## Acceptance Criteria

- The task description and schema distinguish foreground parallelism from deferred background delivery.
- The background launch response no longer encourages an unexplained early final answer.
- Model-facing guidance explicitly discourages repeating delegated scope.
