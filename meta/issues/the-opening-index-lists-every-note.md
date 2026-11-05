# The opening index lists every note

## Summary

A session opens with the newest 20 note lines, at most 4 KB
(`INDEX_NOTES`, `INDEX_BYTES`). Claude Code loads its whole index
(200 lines / 25 KB), so the model always knows what exists and can read
a note itself when recall misses.

## Requirements

- The opening index lists every note, newest first, up to 200 lines and
  25 KB; past that it says how many it left out.

## Acceptance Criteria

- A store test: 30 notes all appear; the cap trims the oldest and says
  so.
