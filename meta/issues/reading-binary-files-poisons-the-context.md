# Reading binary files poisons the context

## Summary

The read tool returns raw bytes for binary files. Observed in the
terranigma session: a subagent read three PNGs (~414 KB of
control-character text) — useless to the model, and brutally
token-dense: one 138 KB read billed 113,456 real input tokens where
the bytes/4 estimate says ~34k. The 4× undercount let the transcript
sail past the model's 200k window without triggering compaction, and
when compaction did run its own request drew HTTP 400 "Prompt
exceeds max length", killing the subagent.

## Requirements

- read sniffs binary content (the image magic sniffer exists; for
  the general case, control-character/invalid-UTF-8 density in the
  head) and returns a one-line description instead of the bytes:
  kind, size, and for images the dimensions — plus a hint that the
  file cannot be read as text.
- Text files with the odd control character still read normally —
  the guard must not catch source code or logs.

## Acceptance Criteria

- Reading a PNG yields a short description, not bytes; reading
  UTF-8 source is unchanged; a truly binary non-image file yields
  the generic description.

## Milestone

12 — Health sweep

## Outcome

`tools/binary.rs` sniffs the first 8 KiB: image magic first (PNG
dimensions from IHDR), then NUL / invalid-UTF-8 / >5% control
density with a 4-control floor; ESC excluded so ANSI logs stay
text. read returns a one-line description with a no-retry hint
instead of the bytes — the 113k-token PNG dump from the terranigma
session becomes ~30 tokens. (35afa82)
