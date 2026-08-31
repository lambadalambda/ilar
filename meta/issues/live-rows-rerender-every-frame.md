# Live rows rerender every frame

## Summary

The render cache is correctly dirty-mark incremental — except for
animated entries. Every entry with a Running/Complete tool or a
running child re-runs `entry_rows` in full at up to 20 fps
(transcript.rs:366-394), *including offscreen entries* and including
expanded args/diff/tail wrapping (with the `full` toggle,
`usize::MAX` rows of detail re-wrap per frame). Memoized child
timelines are handed back by deep clone — the whole
`Vec<TranscriptRow>`, every span String, per frame
(transcript.rs:234-238). And a streaming assistant message re-runs
markdown + wrap over the message-so-far on every delta batch
(transcript.rs:1978-2004) — quadratic over one long reply.

Fix shape: store `Arc<[TranscriptRow]>` in the memo; re-render only
the header/spinner row of an animated entry unless its content
changed; split a streaming entry at the last completed block.

Size: M. Source: sweep 2026-08-31, responsiveness & memory.
