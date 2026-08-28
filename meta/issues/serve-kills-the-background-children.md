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

## Outcome

The drive keeps one `Engine` — runtime, spawner, services, and a
notification consumer — per session, for as long as the process
serves it. Turns reuse it; the per-turn `shutdown()`/`stop_all()`
are gone from the turn boundary and live in `Drive::shutdown`,
which serve runs on Ctrl-C (a select against the axum future,
since the SSE streams never end on their own). The web turn path
and the delivery path share one `run_driven_turn`, so steers, SSE
frames, the failure broadcast and slot cleanup behave identically.

The consumer delivers strictly one at a time: own-session
completions become follow-up turns through the same slot a web
message takes (flat 250ms wait, cancel-aware); foreign ones go
through `route_notification` with exponential backoff, Propagate
fed back around, Requeue retried eight times before leaning on the
outbox. Two red-first tests pin it: a background child's
completion lands in the parent log between turns with no web
request involved, and work spawned in one turn survives the next.

Left for later: SIGTERM (as opposed to Ctrl-C) still skips
teardown; service survival has no dedicated test; the engine does
not yet load outbox pending at adoption — a TUI open of the same
session delivers what serve dropped.
