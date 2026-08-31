# Notices take turns

## Summary

A standing persistent Error swallows later persistent Warnings
(app.rs set_notice_with_lifetime): a stale clipboard error can
permanently hide "background jobs cancelled; task results held —
send a message to deliver" — a gate the user then cannot explain.
The stall-notice guard fixed one instance; the general rule needs
either a small priority queue or a reserved surface for standing
mode reminders (held task results).

A second gap in the same family, found reviewing
[[adoption-waits-for-the-user]]: the held-backlog notice is cleared
by the first `StartTurn`, so message-then-abort leaves the pause
standing with no visible indicator at all until some turn completes.
A reserved surface for standing mode reminders would cover both.

Size: S-M. Source: sweep 2026-08-29, event loop.
