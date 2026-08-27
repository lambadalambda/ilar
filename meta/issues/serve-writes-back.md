# Serve writes back

## Summary

Approved 2026-08-28: phase 3 of the web frontend — the browser can
drive sessions, not just watch them. Send a message to a session,
steer a running turn, abort it, and start a new session from the
page. `ilar serve` becomes a runtime, not just a reader: it embeds
the same headless loop `ilar exec` proved, holding the OS-backed
session writer lock for the sessions it drives. A session open in a
TUI stays watch-only (the writer lock refuses, the page says so).

## Requirements

- POST `/api/sessions` `{prompt, cwd?, model?}` — create a session
  and run its first turn headless; returns the id. POST
  `/api/sessions/{id}/message` `{text}` — steer when a serve-driven
  turn is running (the same steer-vs-queue semantics the TUI's
  decide layer encodes; reuse it, do not reimplement), otherwise
  acquire the writer and run a resume turn. POST
  `/api/sessions/{id}/abort` — cancel a serve-driven turn. Writer
  held elsewhere → 409 with a body the page can show ("open in
  another process — watching only").
- Auth: write routes follow the same token rules as reads (loopback
  free, token elsewhere) — one story, documented in docs/serve.md
  with the safety paragraph updated to say the write path exists.
- Streaming needs no new work: driven turns write the store and the
  live scratch like any turn, and the existing SSE covers them.
- The question tool headless: match whatever `ilar exec` does
  today; an interactive web answer modal is a follow-up, noted.
- UI: an input box in the reserved spot (Enter sends, Shift-Enter
  newline), fate feedback (started / steered / refused), an abort
  control in the status pill while working, a "new session" form
  (prompt, cwd with suggestions from existing sessions, optional
  model), and a watching-only banner on 409 sessions.
- Tests over MockProvider for the turn-driving routes; browser
  verification of the flows; at most a couple of one-line real
  provider calls end-to-end.

## Milestone

11 — Beyond the terminal
