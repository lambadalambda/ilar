# Notices take turns

## Summary

A standing persistent Error swallows later persistent Warnings
(app.rs set_notice_with_lifetime): a stale clipboard error can
permanently hide "background jobs cancelled; notifications paused;
send a message to resume" — a gate the user then cannot explain.
The stall-notice guard fixed one instance; the general rule needs
either a small priority queue or a reserved surface for standing
mode reminders (paused notifications).

Size: S-M. Source: sweep 2026-08-29, event loop.
