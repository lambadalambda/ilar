# Wrapped rows keep their gutter

## Summary

Transcript rendering edge cases, gathered. The big one: rows other
than the assistant's are prefixed (`you  `, `—    `, `  │ `) *before*
wrapping (transcript.rs:1447), so continuation lines of a long user
message, system line, thought or task body start at column 0 under
the label — at 80 columns any two-sentence prompt shows it. Also:

- A parent's `task_message` inside a subagent timeline renders as
  `you  …` although the user never wrote it (transcript.rs:1265,
  session_view.rs:264) — label it after the parent.
- A tool row marked Failed on a settled restore with no result shows
  `×` and `result  pending` (transcript.rs:1684 vs session_view.rs:454).
- The compaction summary is an uncollapsible wall of muted `—` rows
  at the top of every restored session (transcript.rs:1335, app.rs:1253,
  session_view.rs:231) — make it an expandable note.
- A turn-killing error is a `System` line styled like `switched to …`
  chatter (session_view.rs:352, transcript.rs:2222) — give System a level.
- Agent display names clamp to 20 chars, hiding the model override
  (transcript.rs:2436); running-tool rows put progress before the
  command so right-truncation eats the command (2455); collapsed
  groups say `3 calls ✓` and no names (1572).
- `done` for a tool that executed but whose result is not yet
  delivered flips to `×` a frame later (2427).
- markdown: a fixed 24-char rule breaks below 24 columns (markdown.rs:138);
  `2 * 3 * 4` italicises ` 3 ` (583).

Omnibus by design; tick items here.

Size: M. Source: UX sweep 2026-09-03 (transcript).

## Progress (2026-09-05)

Done: continuation rows keep their gutter (`wrap_entry_line`: the
leading label is repeated as blank space, content wraps at the
remaining width) for user, system, task, job and thought rows; a
`System` line that is a turn error paints its gutter in the error
colour and its words in the primary one; a failed row with nothing
recorded says "no result recorded — the turn ended before the tool
returned" instead of `pending`; the markdown rule clamps to the width.
Left: the parent's message inside a subagent timeline labelled `you`;
the compaction summary as an expandable note; the 20-char agent name
clamp; progress-before-command truncation; collapsed groups naming
their tools; `done` flipping to `×`; the `2 * 3 * 4` italic.

## Progress (2026-09-19)

Done: a `*` opens emphasis only when a word follows it and closes only
when one precedes it, so `2 * 3 * 4` is arithmetic again
(CommonMark's flanking rule).

Left, all cosmetic: the parent's message inside a subagent timeline
labelled `you`; the compaction summary as an expandable note; the
20-char agent name clamp; progress-before-command truncation;
collapsed groups naming their tools; `done` flipping to `×`.
