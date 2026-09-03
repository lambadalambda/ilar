# Mail says who, and stays out of the way

## Summary

Cross-session messaging — task completions routed to the session that
spawned them — is hard to read. A task result waiting to be steered
into a turn is listed in the pending strip as its raw
`<task-notification>` envelope under the same `steering · next step`
label as a typed message. Delivery chatter names sessions by the first
eight hex digits of their ids ("delivering "review X" to a1b2c3d4"),
which identifies nothing. And every step of a delivery claims the
notice line above the input, where it competes with things the user
must act on. Reported by the user 2026-09-03.

## Requirements

- The pending strip shows a waiting task or tool result by the same
  collapsed headline its transcript row wears, under its own fate
  label (`task result · next step`, `task result · when the turn
  ends`), never the envelope.
- Sessions are named, not numbered: this session is "this session", a
  running or delivering child is `agent · description` from the
  roster, anything else is `agent · opening prompt` from the session
  head, and only a session with no readable head falls back to its
  short id. One resolver, cached per process, used by every delivery
  message and the focus-view title fallback.
- Delivery chatter leaves the notice line: a delivery starting is
  visible in the agents panel already (its `delivering` row) and gets
  no notice; a delivery completing is one quiet transcript line
  (`✉ "review X" delivered to explorer · survey the API`). Holds and
  failures keep the notice line — those need the user.

## Acceptance Criteria

- Render test: a task-result steer in the strip shows
  `task result · next step: <headline>`; a typed steer is unchanged.
- Resolver tests: own id, roster row, session head, and the fallback.
- Scheduler test: a completed delivery pushes a transcript line naming
  the target and sets no notice.
- docs/interface.md describes the strip labels and where delivery
  status shows.

## Outcome (2026-09-03)

- Strip: a waiting task or tool result is listed as `task result · next
  step` / `task result · when the turn ends` with the headline its
  transcript row wears (`pending_entry` in view.rs, reusing the pending
  manager's headline parser); typed messages are unchanged.
- Names: one resolver (`session_label` in main.rs, behind the scheduler's
  `Runtime`) — "this session", a roster row's `agent · description`, a
  log head's `agent · opening prompt` (48 chars), then `session <id>`;
  cached per process. Every delivery message goes through it.
- Chatter: a delivery starting sets no notice (the agents panel shows the
  `delivering` row); a completed delivery is one transcript line
  `✉ "desc" delivered to <name>`; holds and failures keep the notice
  line, now naming the session.
- Tests: strip rendering, the resolver's four cases, and the scheduler's
  delivered ending landing in the transcript with no notice.

Not done, by choice: the focus-view title fallback (`agent · <id>`) for
a session no longer on the roster still shows the id — it has no store
access, and the roster covers the live case.
