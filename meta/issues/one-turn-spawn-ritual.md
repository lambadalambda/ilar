# One turn-spawn ritual

## Summary

The root-turn spawn sequence — loop-event channel, store rx, fresh
`CancellationToken`, clone resolver/store/session/system
prompt/registry/tool ctx/config, steer channel, `tokio::spawn` into
`TurnCompletion::Root` — is copy-pasted three times in main.rs
(`LoopRuntime::perform`, `start_notification_turn`, the
question-answer resume at main.rs:2130-2164). The question-resume
copy has already drifted: it bypasses `Runtime::perform`'s
`debug_assert!(turn_handle.is_none())` guard and the model-override
block.

## Requirements

- One `spawn_root_turn(kind)` helper; all three sites route through
  it (restoring the guard and override handling to the
  question-resume path).

## Acceptance Criteria

- Existing turn-lifecycle tests pass; question-answer resume honors
  a per-turn model override like the other paths.

## Milestone

12 — Health sweep
