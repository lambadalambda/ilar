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

## Status (2026-08-31)

Partly stale on arrival, partly done since, remainder scoped:

- Already fixed before this sweep: memoized child timelines render
  once, not per frame (`ChildRowMemo`, pinned by
  `an_animating_agent_row_does_not_re_render_its_child_transcript`).
- Landed with the focus work: the focus view's per-event full
  re-render (apply_child_loop_event reports its touched line).
- Remaining, in order: (1) the memo's reuse path still hands rows
  back by deep clone per frame — callers mutate what they get
  (indentation), so the fix is an `Arc`-ified row pipeline or a
  header-only animation pass, M-sized surgery in transcript.rs;
  (2) a streaming assistant message re-runs markdown + wrap over the
  whole message per delta batch (split at the last completed block);
  (3) offscreen animated entries re-render at the busy rate —
  `update()` has no viewport knowledge today.
