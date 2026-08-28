# Serve kills the background children

## Summary

Found 2026-08-28 by audit. `ilar serve` builds a fresh
`SubagentSpawner` **per driven turn** (`drive.rs` plans a new
runtime for every message) and calls `spawner.shutdown()` at the
turn boundary. Nothing in serve ever calls `spawner.subscribe()`.
Consequences, traced:

- A background task that finishes during the turn writes its result
  to its child session log, pushes its notification into a channel
  with no reader, and the whole channel is dropped unread at turn
  end. No follow-up turn ever happens.
- A background task still running at turn end is cancelled by the
  shutdown — silently. The `TurnFailure` broadcast says nothing,
  because the turn itself succeeded.
- The model is actively lied to: the task tool answers "Completion
  will trigger a separate follow-up turn" and background bash says
  "You will be notified when it completes. Do not poll or sleep."
  Neither is true under serve, and the instruction forbids the one
  workaround the model could try.
- A read-only agent's task **defaults to background**, so the
  common delegation hits this without anyone asking for detachment.
- Services started by a web-driven turn are stopped at turn end for
  the same reason: the `ServiceManager` is per-turn too. In the TUI
  both live as long as the session.

`ilar exec` shares the per-turn spawner but at least prints
"{n} background task(s) cancelled at exit" — and a one-shot exec
has nowhere to deliver to, so cancelling is at least defensible.
Serve has a session that continues; it just forgets between turns.

## Requirements

- The serve runtime keeps one spawner (and one service manager) per
  *session* for as long as the process serves it, not one per turn.
- Something in serve consumes the notification channel and delivers
  completions: same-session ones start a follow-up turn through the
  drive (respecting the writer lock and the turn slot), foreign
  ones go through `route_notification`.
- Until that exists, the honest stopgap is the opposite default:
  refuse background execution under serve with a clear tool error,
  so the model runs tasks foreground instead of losing them.
- No serve test currently exercises the task tool at all; the fix
  needs one for a background completion arriving between turns.

## Milestone

11 — Beyond the terminal
