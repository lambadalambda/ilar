# A turn failure waits to be read

## Summary

`TurnFailure` is a one-shot broadcast (drive.rs:764-774): a tab
between reconnect attempts, a waking laptop, or any later page load
sees a transcript that just stops — the exact blindness the frame
exists to cure.

## Fix

Cache the last failure per session in `Drive`; include it in the
transcript response and on SSE attach.

Size: S. Source: sweep 2026-08-29, serve.
