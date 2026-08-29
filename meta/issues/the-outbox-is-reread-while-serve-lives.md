# The outbox is reread while serve lives

## Summary

Adoption reads `outbox::pending` exactly once, at consumer start
(drive.rs:895-903); the bounded retry drops in `follow_up` (~2.5m)
and `route` (~30s) both promise "the next adoption requeues it" —
which, in a long-lived server, is the next restart that may never
come. The durable copy exists; nothing re-reads it.

## Fix

Periodic or drop-triggered outbox re-scan per engine.

Size: M. Source: sweep 2026-08-29, serve + subagent (found twice).
