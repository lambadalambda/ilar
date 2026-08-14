# Minimal TUI: streaming, tool display, input, Esc full-abort

## Summary

ratatui frontend over the core loop. Lean and functional, no session
browser yet.

## Requirements

- Layout: scrollable transcript pane (streaming text, tool calls with
  collapsible output), status line (model, tokens), input box.
- Input: multi-line editing, Enter sends, Esc aborts (loop abort token),
  Ctrl-C quits.
- Rendering driven by `LoopEvent` channel; no blocking of the loop on UI.
- Tool calls render name + one-line summary; output truncated view.
- `--model`, `--session <id>` (resume), `--agent <name>` CLI flags
  (clap, add dep).
- Terminal raw-mode enter/leave with alt-screen; panic-safe restore.

## Acceptance Criteria

- Manual: real session against z.ai or OpenAI with tool calls renders
  correctly, Esc aborts cleanly, session resumable after restart.
- No tests by design; keep TUI layer thin, logic in core.

## Notes

- If ratatui ergonomics fight us, evaluate `ratatui` widget set for
  streaming text (Line/Span buffer reuse). Keep allocation frugal.

## Milestone

1 — Lean core
