# A big draft rewraps per frame

## Summary

The prompt editor wraps the *entire* draft twice per frame —
`visual_line_count` (view.rs:551) and `multiline_view` (view.rs:958)
both call `wrapped_rows` (input.rs:263-329), which builds a
`WrappedGrapheme` per grapheme of every line, though at most
`input_height` rows are visible. And `InputBuffer::insert` recomputes
the cursor by iterating `grapheme_indices` from byte 0 to the
insertion point (input.rs:70-76) — O(prefix) segmentation per typed
character. Paste a 100 KB draft and every subsequent keystroke pays
tens of thousands of grapheme-width computations, twice. Correctness
is fine (ZWJ/combining tested); only the cost is wrong.

Fix shape: cache the wrap per (generation, width) on `InputBuffer`
and share it between the two callers; compute the insert cursor from
the local boundary instead of scanning from 0.

Size: S. Source: sweep 2026-08-31, responsiveness & memory.

## Outcome (2026-08-31)

The wrap is cached on `InputBuffer` per (generation, width) as byte
ranges — the rows borrow `text`, so ranges are what a cache can own —
and both per-frame callers now share one computation. Every text
mutation bumps the generation through `edited()`, enumerated in a
test so a future editing method that forgets it fails. The insert
cursor snaps from the cursor's own line instead of byte zero:
normalization leaves no CRLF, so graphemes never span a newline and
O(line) is exact, not approximate. `PartialEq` became manual — the
cache is a projection, not state.
