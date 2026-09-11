# A turn can be aborted from the chat

## Summary

A gateway turn ran away — thirty thousand tokens of one tool call's
arguments — and the only way to stop it was to restart the service,
which also dropped the messages waiting to steer it.

## Requirements

- `/abort` cancels the running turn on this chat and says so; when
  nothing runs, it says that.
- The aborted turn reports "aborted" rather than "no reply"; the
  messages that were waiting to steer it run as a turn of their own,
  as after a failed turn.

## Acceptance Criteria

- Test: `/abort` during a slow tool call ends the turn, the chat is
  told, and a message that arrived meanwhile is answered afterwards.
