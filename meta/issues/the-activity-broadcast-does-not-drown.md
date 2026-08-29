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
