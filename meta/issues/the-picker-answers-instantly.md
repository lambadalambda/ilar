# The picker answers instantly

## Summary

Requested 2026-08-29 with a screenshot. Four complaints about the
session search modal, and one root cause for the biggest:

- It opens slowly. The empty-query listing (`tail_sessions`) fully
  parses every session log just to excerpt its last words into the
  left column — the excerpt the user wants removed anyway. The
  cheap bounded head-scan (`store.list`) already has everything the
  list itself needs: id, title, cwd, age.
- The most recently used session in the current directory is not
  reliably first.
- The left column is cramped: five one-line hit rows per session,
  each mostly excerpt. Wanted: one entry per session, two lines —
  title (or id) on the first, everything else dim on the second.
  The excerpt is redundant; the preview pane is right there.
- Search should rank title matches above content matches, and the
  preview sometimes shows no visible match at all: context entries
  are truncated from the front, so a hit deep inside a long entry
  falls outside the slice that is shown.

## Requirements

- The empty-query listing paints from head scans alone — no full
  log reads on open. The preview for the selected row loads that
  one session lazily, in the background, newest selection wins.
- Ordering: sessions launched from the current directory first,
  then the rest, most recently modified first within each group —
  and the directory comparison holds up under symlinks
  (canonicalize both sides).
- One row per session in both modes, two lines each: the title (or
  short id) alone on the first line; directory, age, and in search
  mode the match count on the second. Navigation, scroll math and
  the click hit-map account for two-line rows.
- Query mode ranks sessions whose *title* matches above sessions
  with content-only matches, stable within each group by recency.
- The preview centers the hit entry's text on the match instead of
  truncating from the front, and highlights query occurrences in
  the pane, so the reason a row matched is always visible.

## Acceptance Criteria

- Opening the modal on a store with hundreds of sessions shows the
  listing immediately (head scans only), current-directory sessions
  on top.
- Searching a word that appears in one session's title and another
  session's content lists the title session first; selecting the
  content one shows the match highlighted in the preview.

## Milestone

13 — Guard rails

## Outcome

The empty-query listing is one batch built from head scans alone —
`tail_sessions`, which parsed every log to excerpt last words
nobody wanted, is gone. Because the whole listing arrives at once,
the here-first partition is finally reliable too: the old
unreliability was rows streaming in for seconds in global-mtime
order. Both sides of the directory comparison canonicalize, so
symlinked launches still count as here.

Rows are one per session, two lines: the title (query-highlighted)
alone, then directory · age · match count, dim. The preview pane
carries what the excerpt pretended to: for listing rows it loads
the selected session lazily in the background (the loader always
answers, or an empty context would re-spawn it every pass), and
for search rows the hit entry is re-centered on the match —
`around` truncated entries from the front, which is why the pane
sometimes showed no visible reason for the row — with query
occurrences highlighted.

Query mode emits one row per session, and the modal ranks title
matches ahead of content-only matches with the same stable
partition the listing uses for directories. Pinned by
`a_title_match_outranks_a_content_match` and
`the_hit_window_centers_on_the_match`.
