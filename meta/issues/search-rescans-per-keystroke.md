# Search rescans per keystroke

## Summary

Each character typed in transcript search voids every entry's cached
matches and rescans the whole rendered transcript — per row: collect
all spans into a fresh String, `to_lowercase`, `contains`
(main.rs:3587-3594 → app.rs:1514 → transcript.rs:432-437, 524-538).
O(total transcript text) with two allocations per row, per keystroke,
and each keystroke computes twice (`search_refresh` now, `view.rs:585`
next frame after `search_computed_at` was reset — the second pass is
cheap but still walks all entries). Typing a word into a long session
stutters on a slow machine.

Fix shape: cache each entry's lowercased concatenated text beside its
rows; when the new query extends the old (`starts_with` for a
substring search), filter the previous match set instead of
rescanning.

Size: S. Source: sweep 2026-08-31, responsiveness & memory.
