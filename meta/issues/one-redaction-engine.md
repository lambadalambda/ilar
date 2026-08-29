# One redaction engine

## Summary

`sensitive_key` exists twice with different needle lists (turn.rs
has `privatekey`, error_body.rs does not), beside two parallel
token scrubbers with divergent quote/authorization rules — the
two-copies drift that already published a secret once, per
turn.rs's own comments. One `redact` module, both entry points,
one needle table.

Size: M. Source: sweep 2026-08-29, core loop.
