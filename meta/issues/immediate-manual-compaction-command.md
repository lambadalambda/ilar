# Immediate manual compaction command

## Summary

Manual compaction currently arms a forced compaction for the next turn. Run it immediately instead, expose it as `/compact`, and show the generated handover summary in the live transcript.

## Requirements

- Triggering manual compaction performs the complete provider compaction call immediately while the UI is otherwise idle.
- Add `/compact` as a built-in user command for the same operation.
- Display the generated handover summary in the transcript after successful compaction.
- Keep automatic/in-turn compaction behavior unchanged.
- Keep cancellation and errors visible and leave the session valid.

## Acceptance Criteria

- A test proves manual compaction invokes and persists compaction without sending another user message.
- A test proves `/compact` resolves to the immediate manual-compaction operation.
- A render/state test proves the generated summary is visible to the user.
- Workspace formatting, tests, and clippy pass.

## Milestone

7 — Unscheduled
