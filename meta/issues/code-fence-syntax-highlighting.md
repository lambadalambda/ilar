# Syntax highlighting in code fences

## Summary

Fenced code blocks in the transcript render with a gutter but no syntax
highlighting.

## Requirements

- Highlight fenced blocks by their info string (rust, ts, py, go, sh,
  json, toml, md at minimum) using a highlighting crate (syntect or
  two-face), mapped into the active theme's palette rather than shipping
  separate highlight themes.
- Streaming-safe: partial blocks mid-stream must not flicker or panic;
  highlighting may lag until the block closes if needed.
- Unknown/absent language falls back to current rendering.
- Measure binary-size and cold-start impact; if syntect's default dumps
  are too heavy, use a trimmed syntax set.

## Acceptance Criteria

- Snapshot/unit tests for at least rust + json highlighting into themed
  spans.
- No visible regression in streaming rendering.

## Notes

- Lowest priority of the batch; acceptable to defer if the dependency
  weight fights the single-binary ethos.
