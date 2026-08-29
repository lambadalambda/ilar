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

## Outcome

A pure verdict (`stall_verdict`: silence duration, tool-in-flight,
thresholds → Quiet/Warn/Abort) wired after the loop-event drain,
reusing the stream-liveness clock the status line already keeps.
Warn at 300s of true silence — a persistent notice with the count
climbing and one bell; abort at 600s through the Esc-identical
cancel path, so the transcript stays honest and the committed
chain survives. In-flight tools hold the verdict at Quiet — the
child watchdog's silent-tool false positive, deliberately not
copied. Compaction and routed deliveries own no event channel and
are exempt by construction. Thresholds are documented constants;
making them configurable is noted as the natural follow-up.

Review hardening: the warn notice claims the status line only from
nothing, transients, or itself — it may not bury a standing
persistent reminder that the next loop event would then destroy —
and any drained loop event re-seeds the stall clock, so retry
cycles and finishing tools do not count as provider silence.

Known false-abort exposures, accepted for now: a custom
`[models.*]` entry whose server hides reasoning (the chat path
then streams nothing during a long think), and a root in-turn
auto-compaction, which publishes nothing until it finishes while
the watchdog keeps counting — a slow compaction past ten minutes
would be aborted mid-flight, and the "provider silent" warning is
misleading in that window even when the abort never fires. The
fixes belong upstream (a heartbeat or start event around in-turn
compaction; warn-only mode for custom models) and ride with the
configurable-thresholds follow-up.

