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
