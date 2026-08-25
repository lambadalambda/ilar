# The render cache clones the world

## Summary

`TranscriptRenderCache::update` deep-clones the entire transcript
model on every revision bump (`transcript_entries` clones every
line, including nested `child_lines` trees and 16 KB detail
strings), deep-compares against `cached.source`, then clones again
to store — and `push_loop_event` bumps the revision on every
streaming delta. O(whole transcript) allocation + comparison per
token, continuously while streaming; with search open,
`matching_rows` additionally lowercases every rendered row per
delta.

Related: `apply_child_loop_event` (transcript.rs:523-720) is a
~200-line reimplementation of app.rs's `push_loop_event` matching
that has already diverged — the child's `ReasoningSummaryDelta` has
no 64 KB `append_thought_tail` cap (unbounded growth, re-cloned per
cache diff), and tool lookup scans front-first vs the parent's
newest-first.

## Requirements

- Change detection stops deep-cloning per delta (e.g. per-entry
  revision counters or dirty marks instead of clone-and-compare).
- The child event path shares the parent's helpers (thought cap,
  tool lookup) instead of reimplementing them.

## Acceptance Criteria

- A streaming-delta benchmark or test demonstrating no full-model
  clone per delta; a child summary delta respects the 64 KB cap.

## Milestone

12 — Health sweep
