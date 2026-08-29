# Asides do not outlive the switch

## Summary

Every `AppExit::SwitchInto` path cancels routed deliveries and
shuts the spawner down, but none fires `aside_cancel` or joins
`aside_handle`/`topic_handle` (main.rs:2392, switch sites 2664-3344).
A `/btw` in flight keeps streaming tokens for an answer nobody will
ever see.

## Fix

One switch helper (the ritual is duplicated six times — see
[[the-loop-top-joins-the-spine]]) that also cancels and joins the
aside and topic tasks.

Size: S. Source: sweep 2026-08-29, event loop.
