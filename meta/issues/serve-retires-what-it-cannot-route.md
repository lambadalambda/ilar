# Serve retires what it cannot route

## Summary

`Consumer::route`'s error arm logs and returns (drive.rs:1140-1147)
— no `outbox::retire`, no salvage of the child's text. The TUI's
counterpart does both (schedule.rs:344-362). A terminal route
failure (agent removed from config, parent metadata gone) leaves
the entry undelivered forever: every restart adopts, re-fails, and
drops the finished work again.

## Fix

Mirror the TUI arm: salvage the text into the target's log, retire
the outbox entry. Then share the delivered-predicate (three copies
today: outbox.rs:175, subagent.rs:1973, drive.rs:978) so the
pipelines cannot drift further — see [[one-delivery-engine]].

Size: S. Source: sweep 2026-08-29, subagent/routing.
