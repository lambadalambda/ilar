# Plain words on every surface

## Summary

User-facing strings that speak the implementation's language or
disagree with each other. Jargon: `"outbox adoption failed"`
(main.rs:3048), `"the outbox redelivers it when this session next
opens"` (app.rs:1473), `"stall watchdog: provider silent for 600s —
aborting the turn"` (main.rs:2749), `"the turn will retry-resume"`
(2736), `"goal round cap (25) reached without GOAL_ACHIEVED"`
(decide.rs:236, leaks the sentinel), `"resume failed turn from current
context"` (app.rs:1518, modals.rs:805), `"completion arrives as a
notification"` (main.rs:2080), `"cannot reach it while it is busy —
held"` (schedule.rs:342). Inconsistency: `Thought:`/`Thinking:`
capitalised with a colon where every other label is lowercase
(transcript.rs:2120); tools disclose with `▶ ▾ ▼` and everything else
with `▸ ▾`, the third tool state indistinguishable from the second
(2373-2375); `· Thinking: reasoning` vs `thinking…` for one condition
(1269, 1854); `line(s)` pluralisation and "click to expand" where
Enter works too (2529, 1786); the question modal is the only
capitalised title, says `Enter next` on the last question where Enter
submits, and `BackTab` for Shift-Tab (questions.rs:335/421/585-593);
the pending manager footer `Enter edit/act · d delete (×2 for
goal/jobs)` omits services (modals.rs:946); aside says `Esc close` but
Enter closes too (1069); help and todos close on `q` undocumented;
the palette's "Switch session" has a blank shortcut column (559-565).

## Requirements

- Rewrite each in the user's words, naming the key or command where
  the user is told to do something.
- One disclosure glyph pair and one label casing across row kinds.

Size: S, many small edits. Source: UX sweep 2026-09-03 (all three).

## Sweep 2026-09-15 additions

Footers name one key many ways: Enter is `Enter select`
(modals.rs:2934, 3037, 3089), `↵ resume` (2074-2076, 2267), `↵ open
in browser` (1669), `Enter insert` (1223), `Enter save` (3149-3153),
`Enter choose` (grants.rs:30), `↵ keep` (view.rs:172), `Enter
edit/act` (964); a double press is `^D delete ×2`, `↵ rewind (×2
confirms)` and `d delete (×2 for goal/jobs)`; Esc is
close/cancel/undo/deny/back/returns. Pick `Enter` or `↵`, one
double-press phrasing, and `Esc close` everywhere it is not truly
undo or deny.

Done 2026-09-15 (stream C of the sweep): Enter everywhere, one
double-press phrasing, Esc close where it is not undo or deny. The
original list above is still open.

## Outcome (2026-09-20)

**The jargon, all of it.** `outbox adoption failed`, `the outbox
redelivers it when this session next opens`, `stall watchdog: provider
silent for Ns — aborting the turn`, `goal round cap (25) reached
without GOAL_ACHIEVED`, `resume failed turn from current context` and
`cannot reach it while it is busy` each now say what happened rather
than which part of the program it happened in. The `GOAL_ACHIEVED`
sentinel no longer leaks, and a test asserts it cannot.

Two of the issue's entries were already fixed: the warn half of the
stall notice says `Esc aborts, Ctrl-R then resumes the turn`, and
`retry-resume` survives only in code comments. `completion arrives as
a notification` is a doc comment on a field, not a string anyone sees.

**The inconsistencies.** `Thought`/`Thinking` are lowercase like every
other row label, and so is the question modal's title — it was the one
overlay that shouted. Tools disclosed with `▶ ▾ ▼` where everything
else uses `▸ ▾`, and the third triangle was indistinguishable from the
second; there is one pair now and the third state is the word `full`
at the end of the details, where truncation takes it before it takes
the call. `line(s)` made the reader do the grammar. `click to expand`
named one of the two ways. The aside's footer offered `Esc close`
where Enter and `q` close it too. The palette's `Switch session` row
had a blank shortcut column and now names `/sessions`. The question
footer said `next` on the last question, where Enter sends, and called
Shift-Tab `BackTab` — the terminal's name for the key, not the
keyboard's.

Left as it was: the pending manager's footer, which the 09-15 sweep
already normalised, and `Ctrl-N/Ctrl-P` coverage, which belongs to the
help overlay and landed there.

Worth knowing: shortening the question footer was forced by a test
written the day before, which measures the footer against the modal it
is drawn in. `Shift-Tab back` is three cells longer than `BackTab
back` and pushed it to 76 in a 74-cell frame — the footer would have
lost `Esc cancel` at every terminal width.
