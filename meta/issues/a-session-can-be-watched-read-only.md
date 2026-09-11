# A session can be watched read-only

## Summary

Opening a gateway chat's session in the TUI takes its writer lease,
so the chat is paused while you look, and typing drives it. For
debugging one wants the transcript in the TUI, live, without either.

## Requirements

- `ilar --view <session>` opens the session read-only: the restored
  transcript, followed as the file grows, drawn with the TUI's own
  renderer; scrolling, selection and expanding folds work; Enter is
  refused with a notice; no writer lease is taken.
- Whether the session is being driven decides how open rows show.

## Acceptance Criteria

- Builds, opens a session on tenco, and a gateway turn lands in it
  while it is open without the chat being told the session is busy.
