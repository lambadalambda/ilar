# JSON-aware spill previews

## Summary

From the same session: two bash spills (an 83 KiB curl body, a
112 KiB `network requests --json`) each still cost their full
30 KiB preview — a tail-biased window into dense single-line JSON,
which the model then ignored in favour of `jq` on the spill file.
The preview paid ~8k tokens each to say nothing. When the spilled
stream is one complete JSON document, show its shape instead: top
level keys and sizes, and the standing advice to jq the file.

## Requirements

- In the bash spill path only: when stdout was captured completely
  (nothing dropped by tail-biased retention) and parses as JSON,
  replace the stdout preview with a shape sketch — the top-level
  keys with each value's type and size (`object (14 keys)`,
  `array (120 items)`, scalars shown short), an array's length and
  first-element shape, capped to ~2 KiB. The hint line says it is
  JSON and to use jq on the file.
- stderr keeps its tail: errors are prose and the tail is where
  they end.
- A truncated capture (front lost) or anything that does not parse
  falls back to today's tail preview unchanged.

## Acceptance Criteria

- A spilled multi-hundred-KiB single-line JSON object returns
  hint + shape, no 30 KiB tail; a truncated JSON stream and a
  non-JSON stream render exactly as today; a JSON stdout with
  noisy stderr shows shape for one and tail for the other.

## Milestone

13 — Guard rails
