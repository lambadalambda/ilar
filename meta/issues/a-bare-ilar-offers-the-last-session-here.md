# A bare ilar offers the last session here

## Summary

Nearly every launch wants to continue the last session from this
directory, and today that takes `--continue` or the picker. A bare
`ilar` in a directory with a previous root session should offer it
with one key, and show what it was, ghosted, so the choice is made
with the conversation in view.

## Requirements

- On start with no `--session`, `--continue` or `--view`, when the
  per-directory pointer (sessions-list-fast-and-true) resolves to a
  root session with a title: the transcript pane shows that session's
  tail rendered in a ghost style (every colour mapped to the muted
  tone, no bold), under one header line: "previous session here:
  <title> · 2h ago — Enter resumes · type to start fresh · Esc
  dismisses".
- Enter on an empty input resumes it, exactly as the picker's resume
  does. Enter with text sends that text into a fresh session, as
  today. Esc clears the ghost and leaves a fresh, empty session. Any
  other key behaves as usual; the ghost stays until one of those
  three.
- The ghost is the session's tail, not a full replay: bounded by a
  screenful or two of rows from the tail reader, so an 80 MB session
  costs nothing noticeable. Nothing about the ghost is written to any
  log, and the fresh session is not created until something is sent
  (or is removed empty on quit, whichever the store does by then).
- The status line says what is shown ("ghost of <title>") and the
  window title stays plain until a choice is made.
- `general.resume_offer = false` turns it off. Documented in
  docs/interface.md ("Starting") and docs/configuration.md.
- Tests: the offer appears with a pointer and not without one; Enter
  on empty resumes; Enter with text starts fresh; Esc clears; the
  ghost is bounded.

Size: M. Source: user request 2026-09-15. Depends on
sessions-list-fast-and-true (the pointer).
