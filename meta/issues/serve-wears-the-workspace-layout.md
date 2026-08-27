# Serve wears the workspace layout

## Summary

Approved 2026-08-28 from an openchamber screenshot: the serve page
graduates from a rough single column to the three-pane workspace
look — sessions sidebar, dense transcript, session detail panel.
Foundation decision made with the user: vendored preact + htm
(~12 KB, ESM, no build step, no npm; the binary stays
self-contained), because phase 3 interaction lands on these
components.

## Requirements

- Vendored `preact` and `htm` as baked assets served like the
  existing three, listed in PUBLIC_PATHS; pinned versions noted in
  the files themselves.
- Left sidebar: sessions grouped by `cwd` (basename, full path on
  hover), newest first inside a group, liveness dot
  (working/stalled/idle), relative age, click to open; groups
  collapsible; current session highlighted.
- Center: the transcript, denser — one-line tool rows (glyph, tool
  name, muted input summary) that expand on click to detail and the
  full result (existing results route); user/assistant text blocks;
  thinking dim and collapsed by default; the live thinking/working
  pill; the status strip above the (future) input area.
- Right panel: session card — model, cwd, agent; a context bar
  (last usage total vs the model's context limit); subagents from
  the children route with running/finished; compaction count.
- Rust side, contract-additive only: expose the model's context
  limit (meta projection or session summary) so the context bar is
  honest; whatever else the panel needs that the wire already
  carries stays client-side.
- Dark theme by default matching ilar's look, light via
  `prefers-color-scheme`.
- Token auth and the asset exception keep working; serve tests
  extended for new routes/fields; browser-verified with
  screenshots.

## Milestone

11 — Beyond the terminal

## Outcome

Built by an agent, browser-verified against the real store (173
sessions): preact 10.28.1 + htm 3.1.1 vendored verbatim (MIT, ~17 KB
across three ESM files, import map in index.html, PUBLIC_PATHS grown
to exactly the six static assets), app.js rewritten as components
(1146 lines) porting the proven fold/rebase/live-delta logic intact,
app.css to a three-pane custom-property grid (dark `:root`, light
under prefers-color-scheme, single-column with a sidebar drawer
under 760px). Contract addition: `context_limit` on the meta line
AND the session listing row — the input cap via the catalog, same
number the TUI shows, null off-catalog. 12 serve tests, fmt and
clippy clean. Residuals, deliberately unfixed: compaction count is
page-scoped (newest-200 window); the first user message card has no
height cap (parity with the old page); liveness stays scratch-based
so an appending-but-scratchless session reads idle; the context bar
shows the input cap, not the full window — flip in
`view.rs::context_limit` if the other reading wins; agent-browser's
color-scheme emulation is unreliable, so the "dark" screenshots
came out light — the CSS direction was verified by inspection
instead.
