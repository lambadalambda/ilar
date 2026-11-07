# History can show the evidence it finds

## Summary

History search can find text deep inside a long entry, but reading around the event shows only its first 400 characters. Search also stops after 40 matches with no continuation. Both limits can hide the evidence needed to check a memory.

## Requirements

- Add bounded continuation for search results and bounded reads within long entries.
- Let a caller recover the text around a deep hit and then request more context without loading an entire large transcript.
- Keep positions stable across repeated reads and compaction, and keep excerpts valid at Unicode boundaries.
- Preserve the existing current-session access restriction; cross-session access is a separate issue.

## Acceptance Criteria

- A regression test finds text beyond the first 400 characters and retrieves its surrounding evidence.
- A test with more than 40 matches reaches later results without missing or repeating hits.
- Tests cover long tool results, multibyte text, invalid positions and output bounds.
- Existing history queries and event-based context reads remain compatible or have a documented transition.

## Notes

- Part of [the memory follow-ups](memory-follow-ups-from-the-component-review.md).
- `crates/ilar/src/tools/history.rs:15-18,212-221`; `crates/ilar/src/recall.rs:213-276`.
- Share bounded transcript-reading logic with [scoped source retrieval](agents-can-read-scoped-source-transcripts.md) where practical; do not build two readers.
