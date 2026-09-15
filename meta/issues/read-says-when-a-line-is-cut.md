# read says when a line is cut

## Summary

`read_line_prefix` keeps `keep` bytes of an over-long line and
consumes the rest (read.rs:155-175); the loop then breaks with
`truncated = true`, `count_remaining_lines` returns `total ==
last_shown`, and the marker arm `Some(total) if last_shown >= total
=> None` emits nothing (read.rs:195-226). A one-line 1 MiB minified
JSON comes back as `1→{…first ~256 KiB…}` with no ellipsis and no
marker; the model believes it saw the file. A huge line mid-file
gets "continue with offset K+1", skipping the rest of line K.

## Requirements

- When a line was cut, say so: "(line K cut at 256 KiB; the rest of
  that line is not reachable with offset — use bash with jq or cut)".
- A test with a single over-long line and one with a long line in
  the middle.

Size: S. Source: UX sweep 2026-09-15, tools.
