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

## Done (2026-09-21)

All three, one commit each.

**The memo shares instead of copying.** An entry's rows are runs now —
its own rows, or a child timeline held by `Arc` together with the memo
that rendered it. An animation frame on an agent row rebuilds the
header and puts the timeline back by reference count. The test pins
pointer equality between the memo and the entry, not just "rendered
once".

**A streaming reply renders its open block.** The renderer is
line-by-line with one piece of state crossing lines — the code fence
— and a separator is flushed *before* the block that follows a blank
line. So a split at the start of a newline-terminated blank line
outside a fence renders identically in two halves, given a
continuation mode that knows rows exist above it. The memo keeps the
settled prefix's rows (shared, as above) and each delta renders the
tail; the scan for the next split starts at the last one, so a delta
pays for what streamed since it. Exactness is a test over every block
kind at four delta sizes — and it caught two things on the way: a
partially arrived line of spaces reads as blank and then as an indent,
so only a finished line can settle a block; and leading blank lines
settle while drawing nothing, so "continued" has to mean rows were
drawn, not text consumed.

**Offscreen animated entries wait.** `visible_rows` records the
viewport and the animation pass skips animated entries outside it.
The cost is one stale spinner frame when such a row scrolls into
view, at which point it is rebuilt like any other.

From the review: the memo settles by block — one shared run per
settle, so a new paragraph costs that paragraph and not a copy of
every row before it; the carried memo goes only to a reply at the same
line, where before it could be parked unread on whatever entry was
rebuilt first; and a reply with anything after it drops its memo, so a
restored session does not hold every reply's text twice.

Left with [[small-frictions-of-a-long-session]], where it was already
listed: the per-frame bookkeeping scans (`row_count`, `is_empty` and
the like are O(entries) per clean frame).
