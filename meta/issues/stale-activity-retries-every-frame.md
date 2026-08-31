# Stale activity retries every frame

## Summary

`pending_subagent_activity` is capped at 256 entries (app.rs:360,
1212-1225), so it is not a leak — but an activity whose parent row
never appears (a parent this transcript does not host) is retried by
`retry_subagent_activity` every frame for the rest of the session,
and each retry walks the entire transcript recursively per entry
(app.rs:1266-1277; transcript.rs:804-819). 256 stale entries × a
full-transcript walk × 20 fps is a per-frame cost that grows with
the transcript, and stale entries crowd the cap for activity that
could still attach.

Fix shape: age entries out — drop them on the owning child's
`TurnDone`, or after N frames without a match.

Size: S. Source: sweep 2026-08-31, responsiveness & memory.
