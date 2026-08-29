# A queued notification stays collapsed

## Summary

`Steered` and `start_notification_turn` both collapse
task/tool-notification envelopes via `push_notification_row`;
`Intent::StartTurn` pushes the raw text as a user row
(main.rs:624-642). A completion steered into a dying turn gets
requeued and auto-sent — and the transcript shows raw XML where
replay shows the collapsed row.

## Fix

Guard StartTurn with the same
`images.is_empty() && push_notification_row(text)` fold.

Size: S. Source: sweep 2026-08-29, event loop.

## Outcome

`App::push_user_message` is the one fold for a message from the user's
side — typed, requeued, or steered — and both `Intent::StartTurn` and
the `Steered` arm go through it. A typed envelope collapses too, which
is what replay does with it anyway.
