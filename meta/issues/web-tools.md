# Web fetch + web search tools

## Summary

`webfetch` (URL -> markdown text, size-capped) and `websearch` (via a
provider-side search or API like tavily — configurable).

## Requirements

- webfetch: reqwest GET, HTML->text/markdown (scraper crate or similar),
  robots-off (personal tool), truncation, content-type handling.
- websearch: pluggable backend (config `[tools].search` = "tavily" | ...)
  with API key from env; results as structured text.
- Both ReadOnly.

## Acceptance Criteria

- webfetch tested against fixture HTML; websearch tested against mock
  JSON backend.

## Milestone

3 — Polish & extras
