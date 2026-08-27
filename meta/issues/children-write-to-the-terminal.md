# Child processes can write to the terminal

## Summary

Reported 2026-08-28 with a screenshot: `[sudo] password for lain:`
drawn inside the input box. Children get their own process group
but the SAME session, so they keep the controlling terminal — a
program that opens `/dev/tty` (sudo's password prompt above all)
bypasses the captured pipes, scribbles over the ratatui buffer
wherever the real cursor sits, and then blocks the tool call until
its timeout. The corruption also lingers: diff rendering never
repaints cells it believes unchanged.

## Requirements

- `shell_command` children start their own session (`setsid`), not
  just their own group: no controlling terminal, so `/dev/tty`
  opens fail fast — sudo errors immediately with a message the
  model can read and relay instead of hanging. A session leader is
  its own group leader, so `killpg(pid)` reaping is unchanged.
  Covers bash and services in one place.
- Ctrl-L forces a full clear-and-repaint, for whatever still lands
  on the screen from outside (the classic readline chord).
- Note the residual: ilar's own git spawns (checkpoints, worktree
  ops) don't go through `shell_command` and could in principle
  prompt on `/dev/tty` too; scope them separately if it ever shows.

## Acceptance Criteria

- A child running `cat /dev/tty` errors out quickly instead of
  hanging to timeout; group kill still reaps grandchildren.
- Ctrl-L repaints; docs mention both behaviors.

## Milestone

13 — Guard rails

## Outcome

`shell_command` children now `setsid()` in `pre_exec` instead of
`process_group(0)`: a fresh session has no controlling terminal, so
`/dev/tty` opens fail fast — sudo becomes an immediate readable
error instead of a screen-scribble plus a 120 s hang — and since a
session leader leads its own group, `killpg(pid)` reaping is
byte-for-byte unchanged. One helper, so bash and services are both
covered; the fallback arm (setsid failing after fork) keeps the old
setpgid guarantee. Ctrl-L sets `force_full_redraw`, which
`present()` consumes as `terminal.clear()` before the next draw —
the eraser for anything that still lands on the screen from
outside. Test pins /dev/tty failing fast (vacuous where the runner
has no terminal; real when run from one). Residual, noted in the
issue: ilar's own git spawns (checkpoint, worktree ops) bypass
`shell_command` and could in principle prompt on `/dev/tty`.
