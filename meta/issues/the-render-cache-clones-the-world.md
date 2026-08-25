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

## Outcome

The cache now borrows the transcript (`TranscriptEntry<'a>` — the
clone is forbidden by the type system) and re-renders only from the
first dirty line, tracked by chained revision marks whose failure
mode is a full rebuild, never a wrong frame (`mark_dirty_from`
requires contiguous revisions; unmarked bumps degrade safely).
Measured: 811µs of clone+compare per streaming delta became 13.9µs
(~28x with search open, whose per-entry match offsets now cache).
The child event path collapsed onto thirteen shared helpers that
return the lowest changed index — fixing the unbounded child
reasoning growth (64KB cap, red-tested) and unifying tool lookup
newest-first. A frame-for-frame differential net (36-step script x
5 configurations vs a cold cache) plus mutation testing pin it.
Adjacent pre-existing search_refresh staleness fixed alongside.
`App::lines` encapsulation recorded in sweep-cleanups.
