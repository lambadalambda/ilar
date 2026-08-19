# Session housekeeping (delete + fork)

## Summary

Old sessions accumulate with no way to remove them, and exploring an
alternative path requires losing the original.

## Requirements

- SessionStore::delete(id): removes session, replay index, and lock
  files; refuses the active session.
- SessionStore::fork(id) -> new id: copies the log, rewriting the Meta
  event's session_id (append-only format makes this cheap).
- Session picker: Ctrl-D deletes the selected session (second press
  confirms), Ctrl-Y forks and switches to the fork.

## Acceptance Criteria

- Core tests: delete removes files; fork loads and replays identically
  aside from id.
