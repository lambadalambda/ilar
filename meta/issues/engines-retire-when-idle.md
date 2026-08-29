# Engines retire when idle

## Summary

serve inserts engines and never removes them: every session ever
driven pins a runtime, consumer task and per-target workers until
process death. An engine with no running turn, no children and no
services can retire — adoption already handles the cold
re-entry.

Size: M-L. Source: sweep 2026-08-29, serve.
