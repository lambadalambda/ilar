# One delivery engine

## Summary

serve's Consumer re-implements follow-up-vs-route, backoff, hops
and delivered-checks beside the TUI's path — and has already
diverged (no retire, no salvage, adoption-once: three filed bugs).
The delivered-predicate exists in three copies. Extract the
delivery loop into core so both drivers fold the same rules, and
give serve the watchdog it currently lacks entirely (a wedged
provider stream holds a serve slot until a human notices).

Size: L. Source: sweep 2026-08-29, serve + subagent (found from
both sides independently).
