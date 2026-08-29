# The root turn gets a watchdog

## Summary

Child agents have a stall watchdog (`with_stall_timeout`, 600s
default): a child that stops producing loop events is cancelled
and its notification says so. The root turn has none — a provider
stream that silently hangs leaves the TUI at "thinking…" with the
stream-liveness readout climbing forever, and the only exit is the
user noticing and pressing Esc. The activity line's "no data Ns"
is display; nothing acts on it.

## Requirements

- The root turn watches its own liveness the way the child watcher
  does — heartbeats included, so a long silent tool is not a stall
  (the child watchdog's known false-positive on silent tools
  should not be copied; fix the measurement here and consider
  backporting it).
- On stall: surface a notice with the choice (retry/abort), or
  auto-abort into the existing retry-resume path — decide which
  during implementation; both must leave the transcript honest.
- Configurable, defaulting generously (minutes, not seconds).

## Milestone

13 — Guard rails
