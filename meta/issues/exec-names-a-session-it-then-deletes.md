# exec names a session it then deletes

## Summary

`ilar exec` now prints `session <id>` before the turn, and
`docs/sessions.md` promises the id is usable "even if the run is killed
halfway". For a turn that never reaches the provider, it is not.

`run_turn_inner` resolves the provider (`agent/turn.rs:1486`) *before*
it appends the user message (`:1490`), so a bad key or a refused model
leaves a session with nothing in it. `run_exec` then calls
`end_session` (`ilar-tui/src/main.rs:1238`), which removes any root
session with no user message (`session/store.rs:764,783-793`). The id
was already on stdout.

Failure: `ilar exec --json "hi"` with an unset provider key. Line one
is `{"type":"session","id":"X"}`, exit is 1, and
`ilar exec --session X` then fails to resume.

## Requirements

- The id a run advertised is the id a later run can open, or the run
  does not advertise it. Either emit the session line on the first
  event received rather than before the turn, or have `end_session`
  keep a session whose id was published.
- Whichever way: the promise in `docs/sessions.md` holds afterwards.

## Acceptance Criteria

- A test drives a failing provider through `run_exec`'s path and
  asserts that either no session line was printed or the session is
  still there afterwards.

## Notes

- Found by the review of the `small-three` branch, 2026-09-20. Not
  fixed there: it touches `end_session`'s semantics, which the rest of
  the CLI shares.
- Emitting on the first event costs the guarantee that a run killed
  during provider resolution says anything at all, which is what the
  line was for. Keeping the session is probably the better half of the
  trade — an empty session is cheap.
