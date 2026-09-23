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

## Progress (2026-09-20)

Done: a folded group names the tools it called, with a count where one
repeats; a running row leads with its command and keeps its state,
where the two used to be one string cut from the right so whichever
came second vanished whole; and an agent's name is no longer clamped
to twenty characters, which cut exactly the `@model` that says it is
not on the default.

Left, with why — both are larger than "cosmetic" reads:

- **The parent's message inside a subagent timeline labelled `you`.**
  Every restored `UserMessage` becomes `Line_::User`, which has no
  room for a speaker. Labelling it after the parent needs a new `Line_`
  variant, and `Line_::User` is matched in 31 places across five
  files. Worth doing, not worth doing as a one-liner.
- **`done` flipping to `×` a frame later.** `ToolState::Complete`
  means the tool returned and its result has not been recorded yet;
  the end-of-turn sweep marks it `Failed` along with the rows that
  were genuinely killed mid-flight. Keeping it `Complete` instead
  would leave it counted as active — `tool_is_active` includes
  `Complete` — so it would re-render every frame for the rest of the
  session. The honest fix is a fourth state, across 85 match arms, for
  a one-frame icon change on a row whose words already say "no result
  recorded — the turn ended before the tool returned".
- **The compaction summary as an expandable note** is unchanged and is
  the one genuinely worth taking next.

## Progress (2026-09-20, later)

The `you` label is done after all. It needed a `Line_` variant, which
is what I had called too big — and it was three sites, not the
thirty-one I counted: most matches over `Line_::User` are patterns
with a rest, and the two that render it now share one function with a
speaker label as a parameter. `Line_::Incoming` is what a subagent
timeline shows for its parent's `task_message`, labelled `from` and
coloured as delegation rather than as the person.

Note which way the label goes: it belongs to the *timeline*, not to
the event. The same log read as a session in its own right — `--view`
on a child — is still a transcript of user messages, and the test
pins both readings.

Left: the compaction summary as an expandable note, and `done`
flipping to `×` a frame later. The second still wants a fourth
`ToolState` across 85 match arms, because keeping a `Complete` row as
`Complete` would leave it counted as active and re-rendering for the
rest of the session.

## Progress (2026-09-20, later still)

The compaction summary is a folded note. `Line_::Note` renders through
the same `notification_lines` a task and a job row use: a `past `
headline reading "transcript compacted", the handover behind one
keystroke, and the "… N more lines — click or Enter to expand"
affordance the other collapsed rows already carry. Both paths fold it,
the restore and the live `LoopEvent::Compacted`, because both end with
the person looking at a conversation the summary replaced.

Left: `done` flipping to `×` a frame later, which still wants a fourth
`ToolState` across 85 match arms for a one-frame icon change on a row
whose words already say "no result recorded". Judgement, not defect.

## Added by the 2026-09-23 sweep

Markdown continuation rows lose their marker on wrap, seen live at 132
and 80 columns: list items return to the list margin, blockquotes and
code blocks lose `│`, diff lines lose `+`/`-` (the background stays),
JSON args lose their indent. markdown.rs:140-160, text.rs:56
`wrap_markdown_line`, transcript.rs ~2558.
