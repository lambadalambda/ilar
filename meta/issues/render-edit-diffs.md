# Render diffs for edit/write tool results

## Summary

Edit tool results currently render as plain text in the transcript. For a
coding agent, colored ±hunk diffs in the tool row are the biggest remaining
transcript-readability win.

## Requirements

- Compute a unified-style diff for `edit` tool invocations (old_string →
  new_string, with surrounding context from the input) and render it in the
  tool row with themed added/removed line colors.
- `write` that overwrites an existing file should show at least a summary
  (bytes/lines replaced); a full diff is optional if the old content is
  available cheaply.
- Respect the existing three-state tool folding (collapsed → expanded →
  full); the diff belongs to the expanded/full states.
- Diff computation lives in core or a small TUI module as pure functions,
  unit-tested; no external diff binary.
- Colors come from the active theme (all five built-ins).

## Acceptance Criteria

- Unit tests for the diff line classification (added/removed/context,
  truncation of long hunks).
- An edit tool row visibly renders ± lines with distinct colors in the TUI.
- No regression in transcript wrapping/scrolling tests.

## Notes

- Prefer a dependency-free line diff (LCS or similar) in keeping with the
  hand-rolled markdown renderer.
