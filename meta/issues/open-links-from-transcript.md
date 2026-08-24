# Open links from the transcript

## Summary

Agents emit markdown links constantly (`[repo#444](https://…)`), and
the transcript renders them (underlined label, muted URL) but offers no
way to open one. Terminals that auto-detect on-screen URLs give
Cmd+click; keyboard users and everyone else get nothing.

## Requirements

- A link picker (Ctrl-O and a palette entry) over the links in the
  transcript, newest first, fuzzy-filterable; Enter opens the link in
  the system browser and closes the picker.
- Collection covers markdown links and bare `http(s)://` URLs across
  user, assistant, thought, system, task/job, and tool-result text,
  deduplicated by URL (newest occurrence wins).
- Only `http` and `https` URLs are collected or opened — transcript
  text is model-authored, and `file:`/custom schemes handed to the OS
  opener are an attack surface, not a feature.
- Opening uses `open` on macOS and `xdg-open` elsewhere, detached; a
  spawn failure surfaces as a notice (sandboxes may block it).
- Render polish: a markdown link whose label repeats its URL renders
  once, not `url <url>`.
- Works while a turn is running (read-only; no switch guards).
- Help overlay documents the key.

## Acceptance Criteria

- Unit tests pin link collection (markdown + bare URLs, ordering,
  dedup, scheme filtering, tool-output coverage) and the label==URL
  render dedupe.
- The full suite passes; a manual smoke run opens a real link from a
  real session.

## Notes

- OSC 8 terminal hyperlinks were considered and rejected for now:
  ratatui 0.29 has no cell-level hyperlink support, and the
  escape-in-symbol workaround fights selection copy, search
  highlighting, and the theme cell remap.
- The visible muted URL stays: it is what terminals' own URL detection
  Cmd+clicks on.

## Milestone

9 — Time travel follow-ups
