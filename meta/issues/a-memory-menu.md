# A memory menu

## Summary

The person cannot see what memory a session has without asking the
model. Claude Code has a `/memory` viewer. ilar gets a read-only menu
that shows what the current session has access to.

## Requirements

- `/memory` opens a menu with the core block (MEMORY.md, USER.md) and
  every note in the session's store, newest first: kind, age, title,
  summary.
- Notes recalled into this session are marked.
- Selecting a note shows it in full (body included); Esc goes back,
  and closes the menu.
- Read-only. A session without memory says so.

## Acceptance Criteria

- TUI tests: the list renders notes and marks a recalled one; Enter
  shows the body; Esc closes.
- `docs/interface.md` lists the command.
