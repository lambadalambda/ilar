# A share taken mid-turn says it is idle

## Summary

`Share` is not in `palette_command_blocked`, so it can be taken while
a turn runs. The file then records `"state": "idle"` for a session
that is not, and `project_page(…, live = false)` renders the in-flight
tool call as one nothing will ever answer — a ✗ on a call that was
fine.

Two honest readings, and the choice is the point:

- A share is a *snapshot*, and a snapshot of a running session is
  allowed to say "this is where it had got to". Then the file should
  say so rather than claiming idle.
- Or a share is of a *finished* session, and the command waits.

## Requirements

- Pick one and make the file's words match it.

## Acceptance Criteria

- A test shares a session with an unanswered call and asserts the file
  does not describe it as settled and failed.

## Notes

- Found reviewing the share feature, 2026-09-21.
- The Markdown export has the same question and answers it by taking
  the live rows off the screen; a share reads the log, so it cannot.
- Size: S. Related: [[a-session-shares-as-one-html-file]].
