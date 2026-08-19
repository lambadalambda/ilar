# Session list + resume-last

## Summary

Sessions are resumable only via `--session <id>`; the store has no list
API and there is no way to discover or reopen recent sessions from inside
the TUI.

## Requirements

- Session store: a list API returning id, mtime, and a display title (first
  user message, truncated) without fully replaying each session.
- CLI: `ilar --continue` resumes the most recently modified session in the
  current session root.
- TUI: a session picker (command palette entry) listing recent sessions by
  title + relative age; selecting one switches to that session.
- Ordering: most recently modified first.

## Acceptance Criteria

- Unit tests for the list API (ordering, title extraction, tolerance of
  corrupt/empty session files).
- `ilar --continue` with no sessions exits with a clear message.
- Picker opens from the palette and resumes the chosen session.

## Notes

- Title extraction should read only as much of the JSONL head as needed.
