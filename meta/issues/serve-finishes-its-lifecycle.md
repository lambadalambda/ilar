# Serve finishes its lifecycle

## Summary

Residue from serve-kills-the-background-children, noted in its
outcome and the review that gated it:

- SIGTERM skips teardown entirely — only Ctrl-C (SIGINT) runs
  `Drive::shutdown`. A systemd stop or `kill` leaves background
  children cancelled by process death instead of grace, and says
  nothing.
- An engine adopts no outbox entries when it starts. Completions a
  previous process recorded for a session serve now drives sit on
  disk until a TUI happens to open that root; serve itself should
  requeue them at adoption.
- The delivery consumer is strictly serial per engine: one foreign
  delivery waiting on a busy child's claim stalls that engine's
  whole queue, own-session completions included. Availability
  only — nothing is lost — but a long child turn can hold every
  other result hostage for its duration.
- Service survival across driven turns has no dedicated test; it
  is implied by the engine lifetime and nothing pins it.

## Requirements

- A SIGTERM listener runs the same teardown as Ctrl-C.
- Engine adoption calls `outbox::pending` for its session tree and
  feeds the result through the consumer.
- Deliveries that only wait (claim, lease) must not block
  deliveries that could proceed — per-target serialization instead
  of per-engine, or a waiting delivery yields the loop.
- A test pins a service started in one driven turn answering in
  the next.

## Milestone

11 — Beyond the terminal

## Outcome

SIGTERM joins Ctrl-C in one shutdown_signal (unix-gated, degrades
loudly). Engine adoption runs `outbox::pending` once at consumer
start and feeds recovered completions through the live dispatch.
The serial consumer became a dispatcher: parcels go to per-target
workers (serial per session, concurrent across them), propagations
re-enter with a bounded hop budget, teardown joins workers under
the shutdown grace. A real service started in one driven turn
answers `status`/`logs` in the next, pinned.

Review hardening after the fact: `follow_up` now runs the same
delivered-check `route_notification` performs — an adopted entry
racing another process cannot append twice — re-checked after
every backoff; and the hop budget counts routes remaining, not one
extra. Known narrow races left in the outbox files themselves
(retire-vs-pending sidecar sweep, record-vs-compaction rewrite):
cross-process only, self-healing or bounded to one re-announce,
documented in outbox.rs.
