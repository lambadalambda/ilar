# The footer names keys the terminal cannot send

## Summary

Two input affordances are advertised where they cannot work, and one
that does work is advertised nowhere.

**Shift-Enter.** Without the kitty keyboard protocol a terminal sends
the same byte for Enter and Shift-Enter, so `handle_prompt_key`
(`input.rs:491`) never sees the modifier and the draft is sent instead
of gaining a line. `main.rs:327` only pushes the disambiguate flag when
`supports_keyboard_enhancement()` says yes, and inside a tmux with
`extended-keys off` — tmux's default — it says no. The prompt footer
(`view.rs:1113,1122,1124`) and the question modal
(`questions.rs:335`) name `Shift-Enter/Ctrl-J` unconditionally. The
help overlay already has the mechanism for this: `portable_keys`
(`modals.rs:777`), used for `F2`, shows a different key when the
terminal cannot report the chord. The newline binding
(`modals.rs:810`) does not use it.

**Marking text with the mouse.** `main.rs:336` enables mouse capture
unconditionally, which is what a transcript selection needs, but it
also takes the terminal's own selection away for as long as ilar runs.
Holding Shift while dragging bypasses mouse reporting in every terminal
worth naming. Nothing in the help, the footer or the docs says so, and
a person who wants the terminal's selection rather than ilar's has no
way to find that out.

## Requirements

- A key that the terminal provably cannot report is not offered. The
  newline binding uses `portable_keys`; the prompt and question footers
  say `Ctrl-J` alone when the keyboard is not enhanced.
- The help overlay and `docs/interface.md` say that Shift-drag gives
  the terminal's own selection back.

## Acceptance Criteria

- A test pins the footer and the help naming `Ctrl-J` alone when
  keyboard enhancement is off, and both keys when it is on.
- A test pins the Shift-drag line in the help.

## Notes

- Found 2026-09-20, chasing a report that newlines and mark-to-copy did
  not work on tenco. The cause there was environmental —
  `extended-keys off` in that box's tmux, since fixed in its
  `~/.tmux.conf` — but ilar advertising a key it cannot receive is what
  made it look like a bug in ilar.
- Not in scope: a setting to turn mouse capture off. Shift-drag is the
  standard answer and costs nothing.
