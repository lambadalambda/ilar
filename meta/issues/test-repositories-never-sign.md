# Test repositories never sign

## Summary

Scratch-repository helpers set a local Git identity but inherit global `commit.gpgSign`. On a developer machine that signs commits, tests invoke the real signer and fail or prompt while creating fixture commits. The workspace suite currently fails this way unless signing is disabled through environment config.

## Requirements

- Make every temporary Git repository explicitly disable commit signing before its first commit.
- Centralize the fixture setup where practical so new repository tests inherit the hermetic behavior.
- Do not alter the developer's real repository or global Git configuration.

## Acceptance Criteria

- Repository tests pass with global `commit.gpgSign=true` and an unavailable or refusing signer.
- No test invokes a signing agent.
- Existing workspace, checkpoint, rewind, background, and subagent Git tests remain green.

## Notes

- Source helpers include `crates/ilar/tests/background.rs:673-695`, `workspace.rs:7-33`, `checkpoint.rs:6-65`, `rewind.rs:9-35`, and `subagent.rs:529-577`.
- Reproduced during the current codebase review; `commit.gpgsign=false` made the failures pass.
