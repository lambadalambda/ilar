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
- **Pause-path residuals** (found while fixing
  server-pauses-lose-usage-and-content, deliberately deferred): a
  cancel with a *contentless* pause still drops usage (persist is
  gated on `!paused_content.is_empty()`); `persist_failed_step` and
  the abort branch are ~40-line near-duplicates; the
  `announced_calls`-minus-`completed_ids` loop in turn.rs is dead
  (`start_tool_call` covers every announced id); abort/error paths
  publish no `StepComplete`, so live TUI totals and reloaded totals
  diverge by the recovered usage.
- **Small bugs**: the mouse wheel is inert in the pending manager,
  which scrolls since 0d9737d — wire it to `move_selection` and
  drop the stale "handful of rows" comment (app.rs:527-531);
  uncataloged zai models are asymmetric after
  3289226 — the wire reserves the 16k output floor but
  `Config::input_limit`'s `fallback_context_limit` path reserves
  nothing (pre-existing; fix means touching the fallback path);
  web.rs `decode_entities` replaces `&amp;` first
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
