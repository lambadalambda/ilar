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
