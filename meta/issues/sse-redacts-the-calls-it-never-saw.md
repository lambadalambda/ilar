# SSE redacts the calls it never saw

## Summary

The SSE `Feed` starts with an empty `call_inputs` map and throws
away the map `catch_up` harvests (http.rs:873, 900). A tool call
already on the page when the stream attaches → its later result
frame is redacted with `Null` input: argument secrets (a
`--api-key=…` echoed by the command) reach the browser unscrubbed.
The full-page projection redacts correctly; only the live frame
leaks.

## Fix

Seed `Feed.call_inputs` from the session read the route already
performs, and keep catch_up's harvest.

Size: S. Source: sweep 2026-08-29, serve. Security-relevant.
