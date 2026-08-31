# Every started turn publishes its ending

## Summary

`run_turn_inner` publishes `TurnStarted`, then uses `?` on initial or in-turn compaction and synchronous `Provider::stream` construction. Those failures return without the reserved terminal `TurnDone`, violating the loop-event channel contract. Driver-level cleanup hides much of the TUI impact, but core event consumers can still observe a bare close after a started turn.

## Requirements

- Route every error after `TurnStarted` through one finalization path.
- Preserve valid session state and close any announced tool calls when applicable.
- Publish exactly one terminal `TurnDone` on every post-start return path.
- Keep pre-start failures distinguishable as `TurnNeverStarted`.

## Acceptance Criteria

- Fault-injection tests cover synchronous provider preflight failure, compaction failure, and a representative persistence failure after the initial user message committed.
- Every tested post-start error observes one `TurnStarted`, one terminal `TurnDone`, and no events after it.
- The implementation makes the terminal-event guarantee structural rather than relying on an audited list of `?` sites.
- Existing cancellation, provider-stream-error, and retry behavior remains green.

## Notes

- Source: `crates/ilar/src/agent/turn.rs:1556-1590`, `1692-1711`, `1733-1745`; channel contract in `crates/ilar/src/agent/event.rs`.
- This is a residual lifecycle hole, not the already-completed stream-error cleanup.
