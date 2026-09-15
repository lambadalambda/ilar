# Retry is armed when it is promised

## Summary

- The stall notice "provider silent for {N}s — Esc aborts, the turn
  will retry-resume" (main.rs:2888-2890) is false: Esc or the
  watchdog's own abort ends in `TurnDone { Aborted }` → "turn
  aborted"; `retry_available` is set only in the `Err` arm of
  `finish_turn` (app.rs:1597), so Ctrl-R does nothing and the pending
  manager shows no retry row.
- A session resumed after a failed turn cannot be resumed: the
  restore path replays `TurnError` as a plain System line
  (session_view.rs:424-430) and never arms retry; docs/interface.md:76-78
  says "Ctrl-R resumes from the same committed state".
- Ctrl-R with nothing to resume is silent (main.rs:4392-4396).

## Requirements

- Arm the resume on an abort whose chain is committed and on restore
  after a TurnError, or narrow the notice and the doc. "nothing to
  resume" when there is nothing.

Size: S. Source: UX sweep 2026-09-15, session lifecycle.
