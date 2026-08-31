# The transcript never forgets

## Summary

`App.lines` grows per event, forever — the only pruning in the live
path is the 64 KiB thought tail and incomplete-thought removal, and
context compaction just pushes a System line without trimming
anything (app.rs:187, 1173-1182); the compaction cut is applied only
on restore (session_view.rs:177-218). Each `Line_` pays the Tool
variant's ~400 B stack size, restored tool results keep up to
`MAX_KEPT_RESULT_CHARS` = 256 KiB apiece (transcript.rs:1143), and
`TranscriptRenderCache.entries` keeps a rendered, span-fragmented
projection of all of it beside the model (transcript.rs:115-147) —
so every retained byte costs roughly 2-3×. An 8-hour heavy session
sits around 25-50 MB and only quitting resets it; restoring a session
full of big results can jump tens of MB at once.

Fix shape: when a `Compacted` event lands, trim the model the way
restore does — drop or summarize entries behind the cut, or at least
shed `result`/`argument_detail`/`diff` payloads from rows older than
it; the render cache follows the model for free.

Size: M. Source: sweep 2026-08-31, responsiveness & memory.
