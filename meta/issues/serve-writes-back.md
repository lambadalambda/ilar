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

## Outcome

`serve/drive.rs` (815 lines, over half of it tests): serve embeds
the exec-proven headless loop behind `Drive`, holding the OS-backed
writer lock for sessions it runs — the 409 decision is made under a
lock actually held, and a finishing turn removes only its own
registry entry (epoch-tagged; the agent caught its own eviction
race in self-review). Steer-vs-queue is `crate::decide::
submit_target` itself, not a fork; serve has no queue, so an
unreachable Queue target is a loud 500 rather than a silent
second turn. Listing rows gained `driven` beside `state` — a
session can be working under a TUI and must not offer a stop
button. Question tool matches exec: never registered, immediate
failure, web modal filed as follow-up. UI: composer with fate
feedback and watching-only banner, stop button while driven, new
session form with cwd datalist. 16 integration tests (writer-held
409, token on writes, no-provider failure isolated to the request)
plus 7 in-process MockProvider tests through the real router —
steering asserted on the provider's own next-step request.
Browser-verified end-to-end with 2 authorized one-line real calls
against an isolated ILAR_STATE_DIR (the sandbox cannot write the
real store — itself a nice confirmation of the lock model), state
cleaned after. Residuals: no question modal; `with_resolver` test
seam covers the top-level turn only; driven turns keep no
server-side progress log beyond store+scratch; resume never
overrides the persisted model; a configured OpenAI reasoning
variant collides with non-OpenAI models typed into the form
(pre-existing exec behavior, error shown honestly).
