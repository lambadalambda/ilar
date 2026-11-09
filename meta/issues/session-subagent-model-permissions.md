# Session-scoped subagent model permissions

## Summary

Subagent model permissions must be saved with each session, not as a global user preference.

## Requirements

- Save model-menu permission changes in session state; restore them on resume.
- New sessions default to only the immediate parent's current model.
- Changes in one session do not change permissions in another.
- Child agents use the owning session's policy, including nested and recovered work.
- Forks retain the source session policy; reset restores the same-as-parent default.
- Remove global allowlist persistence and update documentation.

## Acceptance criteria

- Tests cover separate sessions, restart/resume, reset, and fork behavior.
- Existing model restrictions and picker tests pass.
