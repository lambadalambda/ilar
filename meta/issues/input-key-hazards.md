# Input key hazards and busy-state affordances

## Summary

Four independent input-handling defects in the TUI, all of which either
swallow a keystroke the user meant literally or misreport what the UI
will accept.

**Bare `r` resends the last prompt** (`main.rs:8343`). Armed after any
turn error and stays armed until the next submit. Typing "run the tests"
as the recovery message fires a full retry of the *previous* prompt on
the first keystroke. Same class: `?` opens help whenever the input is
blank (`main.rs:8275`), so no message can begin with `?`.

**No cursor while a turn runs.** `set_cursor_position` (`main.rs:4380`)
and the focused input border (`main.rs:4253`) are gated on `!self.busy`,
but typing while busy *works* — it queues the message. The user gets
text with no caret and an unfocused border, i.e. the UI signals "not
accepting input" while accepting input.

**`Ctrl-M` is documented but sends the message.** The help overlay lists
"Ctrl-M / F2 switch model". Crossterm special-cases `b'\r'` to
`KeyCode::Enter` before the control-character branch
(`crossterm-0.28.1/src/event/sys/unix/parse.rs:92`), so without the kitty
keyboard protocol Ctrl-M *is* Enter. On terminals lacking enhancement
(Terminal.app among them) the documented shortcut fires off the draft.

**Ctrl-D/Ctrl-U asymmetry.** Ctrl-U is guarded by `input.is_blank()` so
it falls through to readline kill-to-line-start while typing; Ctrl-D
(`main.rs:8311`) has no guard and always scrolls, so forward-delete never
reaches `handle_prompt_key`.

## Requirements

- No bare printable character triggers an action while the prompt has
  focus. Retry moves to a modifier chord (the Ctrl-Q pending manager
  already offers it) or is scoped to a short window after the error.
- `?` only opens help when it cannot be a message the user is typing.
- The caret is visible and the input border reflects focus whenever
  keystrokes reach the input buffer, including during a turn. The queued
  destination should be evident (the title already shows "N queued").
- Ctrl-M is either conditioned on `keyboard_enhanced` or dropped from the
  help overlay; F2 stays the portable binding.
- Ctrl-D scrolls only on a blank input, matching Ctrl-U, so forward-delete
  works while typing.

## Acceptance Criteria

- Test: with `retry_available` set and a blank input, `r` inserts the
  character and does not start a turn.
- Test: `handle_prompt_key` receives Ctrl-D when the input is non-blank
  and deletes forward.
- Test: the help overlay's model-switch binding does not advertise Ctrl-M
  when keyboard enhancement is unavailable.
- Manual: typing during a running turn shows a caret and a focused border.

## Notes

- The retry affordance itself is fine and discoverable — the error notice
  says "press r to retry". Only the binding is wrong; the arming window
  outliving the notice is what makes it dangerous.
- Ctrl-J newline is unaffected: crossterm maps `0x0A` in raw mode to
  `Char('j')` + CONTROL, so it works without enhancement. Shift-Enter
  does require the kitty protocol, but Ctrl-J is already shown alongside
  it in the input hint.

## Milestone

6 — Hardening
