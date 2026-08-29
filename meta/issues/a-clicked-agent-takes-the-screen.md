# A clicked agent takes the screen

## Summary

Subagent work is cramped into nested preview timelines under the
parent's tool rows. Claude Code's agent list does one thing better:
click an agent and you are *there*, reading its transcript full
screen. Everything needed exists — children are real sessions in
the store, `restored_session_view` replays one, and the
`SubagentActivity` broadcast already delivers every child
`LoopEvent` tagged with its session id. Focus is a filter over a
stream we already have.

## Requirements

- Clicking an agent row (from [[the-agent-panel-is-a-tree]]) opens
  a read-only, full-screen focus view of that agent's transcript:
  seeded by store replay, then followed live from the
  `SubagentActivity` feed filtered by `child_session_id`.
- Grandchild activity nests inside the focused transcript via the
  existing `apply_subagent_activity` fold, with the focused session
  as root.
- Esc (or clicking "main") returns to the root transcript, which
  kept flowing untouched — focus is a view, not a transfer: the
  watchdog, notices, deliveries and steers all stay with the root.
- The view scrolls like the picker preview; input is visibly
  routed nowhere (a hint line), not silently swallowed.
- A focused agent that finishes says so in place; the view does not
  vanish under the reader.

## Acceptance Criteria

- Focus on a live child shows its history and its next streamed
  events without a restart.
- The root transcript is byte-identical before/after a focus
  round-trip.
- A finished/aborted focused child shows its ending; Esc still
  returns.
- Focusing an agent of a foreign session (grandchild, sibling tree)
  works or refuses with an honest message — never a blank screen.

## Notes

- Render as a full-screen modal like the picker preview rather than
  teaching the main transcript to swap buffers: selection/search
  stay root-owned, read-only semantics come free, Esc already
  means "close".
- Steering the focused agent is deliberately out: see
  [[steer-the-agent-you-are-looking-at]].

## Outcome

Clicking an agent row opens its transcript over the transcript
area — seeded by `restored_session_view_with_store` (grandchild
history included), then followed live from the `SubagentActivity`
broadcast: the focused id folds flat, everything deeper nests via
`apply_subagent_activity` with the focused session as root, and a
sibling's stream provably cannot leak in. The view owns its own
render cache, so it draws exactly like the main transcript. Esc
closes it and structurally cannot reach the turn-abort arm; a
question modal still renders above and takes keys first; Ctrl-C
rides the Esc path; the root transcript is test-proven
byte-identical through a round trip.

Design calls: the sidebar stays visible (it is the map — "main"
and other agents stay clickable, so focus can hop directly), and
input is visibly inert with the footer saying read-only. A
finished agent keeps the view up with an honest footer, and a
`task_message` resume flips it back to live.

Accepted gap, filed as [[focus-seeds-the-step-in-flight]]: the
store commits step by step, so focusing a mid-step agent misses
the step in flight — a seam line says so, and a never-seen tool
that finishes gets a row synthesized rather than vanishing.
