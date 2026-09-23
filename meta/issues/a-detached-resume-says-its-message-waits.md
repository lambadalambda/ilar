# A detached resume says its message waits

## Summary

Since [[a-message-to-a-finished-task-runs-detached]], `task_message`
to a finished task resumes it in the background. When that resume ends
before its turn starts — the lease wait is cancelled or refused, or
the turn declines before appending its prompt — the message stays
queued for the task's next resume, but the failure notification does
not say so. The foreground path appends "it is not lost". The model
reads the detached failure as a lost message and sends it again.

## Requirements

- A detached task's notification says when messages to it are still
  queued, and that they are delivered at its next resume.
- Counted after the steer hold has handed back what it took, so a turn
  that never started counts its folded-in messages.

## Acceptance Criteria

- A test: a detached resume cancelled while it waits for the lease
  notifies with the queued note.
- Full gate green.

## Notes

- Source: review of the detached resume, 2026-09-23. Size: S.
