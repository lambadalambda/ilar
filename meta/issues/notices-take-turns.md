# Notices take turns

## Summary

A standing persistent Error swallows later persistent Warnings
(app.rs set_notice_with_lifetime): a stale clipboard error can
permanently hide "background jobs cancelled; task results held —
send a message to deliver" — a gate the user then cannot explain.
The stall-notice guard fixed one instance; the general rule needs
either a small priority queue or a reserved surface for standing
mode reminders (held task results).

A second gap in the same family, found reviewing
[[adoption-waits-for-the-user]]: the held-backlog notice is cleared
by the first `StartTurn`, so message-then-abort leaves the pause
standing with no visible indicator at all until some turn completes.
A reserved surface for standing mode reminders would cover both.

Size: S-M. Source: sweep 2026-08-29, event loop.

## Sweep 2026-09-03 additions

- The gate at app.rs:2077-2084 also drops the quit warning
  (`"N stashed prompt(s) would be lost … — Ctrl-D again quits"`,
  main.rs:3302) while `quit_armed` still arms — the second Ctrl-D
  quits unwarned behind a stale clipboard error. Same for `"turn
  aborted"` and `"N undelivered steer(s) moved to the queue"`.
- Any notice replaces the whole status line — model, in/out, cache, Σ
  (view.rs:189-227); persistent ones hide it indefinitely. Give
  notices their own optional row (the strip already grows that way).
- Informational notices to demote or drop: `compaction starting /
  complete / nothing to compact / aborted`, `asking aside…`, `opened
  {url}`, `services stopped`, `removed queued message`, `theme saved`,
  `transcript exported`, `goal achieved`, `running in the background`
  (several are already transcript lines too), `image attached …`
  (duplicates the strip row), `Ctrl-X: M models · T themes`, `input
  stashed (1)` (the title badge already says so).
- A merely held result claims a persistent warning and flips the
  activity to Paused (schedule.rs:369-380): transcript line plus a
  `· 1 held` title badge instead; notice only for Salvage/Exhausted.
- The input title accumulates `· 2 steering · 4 queued · 2 stashed ·
  goal 3/25` (view.rs:872-884); steering/queued duplicate the strip,
  goal duplicates the sidebar. Keep the line counter and `stashed`.

## Progress (2026-09-04)

Done: errors are transient unless a turn died, compaction failed or
the process crashed — those three stay standing; a standing notice is
replaced by any newer standing notice (a held-results reminder no
longer hides behind a stale error) and never by a transient one; the
quit warning always shows (`set_notice_now`); the confirmations that
already had a transcript line or a strip/title badge lost their notice
(exported, goal achieved, running in background, compaction aborted,
image attached, input stashed); the input title stopped counting
steers and queued messages the strip lists. Left: a reserved row for
standing reminders so the status line keeps model and cost beside
them, and the message-then-abort case that leaves the pause with no
indicator — both want that row.

## Outcome (2026-09-05)

The reserved row exists: a notice renders on its own line between the
status line and the pending strip (`notice_line` in view.rs), so the
model, the usage and the meter stay beside it at every width, and the
status line's activity detail yields as the line narrows. The second
gap is closed structurally: with delivery paused and results held, the
row shows "N task result(s) held — send a message to deliver" derived
from the runtime's state, whether or not a notice stands. Precedence,
transient errors and the dropped duplicates landed on 2026-09-04.
