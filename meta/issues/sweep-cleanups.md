# Sweep cleanups

## Summary

Low-severity items from the health sweep, batched. Each is small,
verified, and safe to do opportunistically:

- **Dead code**: `compaction::compact_if_needed` (no callers);
  `PaletteAction` single-variant wrapper; auth.rs `expires_at`
  written but never read (401-driven refresh only — either use it
  proactively or drop it); `login_flow`'s redundant post-exchange
  `account_id` recompute; `Loader::with_env`'s vestigial second
  parameter; `ModelInfo.max_context_limit` read only by a test;
  transcript.rs `tool_line` + the `Line_::Tool` arm of
  `transcript_entry_lines` are production-dead test seams (mark or
  remove).
- **Duplication**: goal-abort message built in two places
  (main.rs:282-291 vs 2189-2196); maintenance-command arg
  validation in both decide::submit and prepare_prompt with drifted
  messages; session-resume validation duplicated between picker and
  search resume arms (four copies of one error string); search-scan
  teardown repeated in four arms; two `centered_rect`s
  (modals.rs vs questions.rs) and questions.rs hand-building modal
  chrome; questions.rs Single/Multiple key arms; the scroll-clamp+
  skip+take triple in render_help/todos/aside; ThinkingDelta vs
  ReasoningSummaryDelta append-or-create; `slash_candidates` vs
  `resolve_slash` builtins merge (and `slash_inventory`'s doc lies —
  SkillPicker consequently omits builtins like `/goal`).
- **Small bugs**: web.rs `decode_entities` replaces `&amp;` first
  (double-decode; do it last); `login_flow` uses macOS-only `open`
  (use a platform opener or document); markdown/text tab expansion
  counts chars not display width; text.rs hard-wrap sentinel is a
  magic string+color pair (share a constant); questions.rs
  `handle_key` indexes without the empty guard render has;
  `row_count()` never returns 0 (`TAIL_PADDING_ROWS`), defeating
  emptiness checks; project-level `general.theme` silently ignored
  (warn); `parse_table`'s no-op truncate; session_view's
  loop-invariant compaction guard skips child summary markers.

## Requirements

- Work through the list in small topical commits; drop any item
  that turns out to be intentional (note why here).

## Acceptance Criteria

- Each addressed item covered by existing or new tests where
  behavior changes; the list above annotated done/kept.

## Milestone

12 — Health sweep
