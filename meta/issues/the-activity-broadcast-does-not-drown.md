# The activity broadcast does not drown

## Summary

One 256-slot broadcast carries per-token deltas from every child at
every depth (subagent.rs:21, 558); the TUI treats `Lagged` as
end-of-drain (main.rs:2607-2614). Several streaming children →
the live tape silently gaps. Correctness survives (registry rows,
sender-side delivered checks), but the nested previews and the new
focus view stutter exactly when the most is happening.

## Fix

Coalesce deltas sender-side, or split deltas onto a lossy channel
distinct from lifecycle events; treat `Lagged` as resync-not-break.

Size: S-M. Source: sweep 2026-08-29, subagent.

## Outcome

The feed is four times bigger (1024 slots — tokio allocates the ring
eagerly and a slot can hold a 16 KiB delta, so this is a bound, not a
number to keep raising), and a `Lagged` is a gap rather than an end:
the drain skips it and keeps going, where it used to stall for the rest
of the frame exactly when several children were streaming. Sender-side
delta coalescing was left alone — with the drain no longer capped below
the ring it is a CPU question, not a correctness one.

While in there: `push_subagent_activity` retried the held-activity
queue on every event, which made a busy frame quadratic in the
transcript's length. The retry runs once per frame now, after the
drain.
