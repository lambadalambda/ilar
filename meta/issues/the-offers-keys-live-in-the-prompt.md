# The offer's keys live in the prompt

## Summary

The resume offer explains itself in its header — "previous session
here: <title> · just now — Enter resumes · type to start fresh · Esc
dismisses" — which wraps to two lines, reads as a sentence about three
keys, and puts the instructions furthest from the place they are
obeyed. The keys belong under the cursor, muted, in the empty prompt;
the header should say only what is on screen. And the offer should
answer the first keystroke rather than wait for Enter: typing is
already the choice.

## Requirements

- The header says what it shows and nothing else: "previous session
  here: <title> · 2h ago".
- While the offer is up and the prompt is empty, the input field shows
  a muted placeholder: "Enter resumes · type to start fresh". It goes
  the moment anything is typed.
- The first character typed dismisses the offer — the ghost vanishes
  as the draft starts, and the character lands in the prompt as usual.
  Esc on an empty prompt still dismisses; it is no longer advertised.
- Keys that neither type nor answer (scrolling, Ctrl-P, the Ctrl-X
  leader, a modal in front) leave the offer alone, as today.

## Acceptance Criteria

- Typing one printable character with the offer up leaves a one-
  character draft and no ghost.
- Enter on an empty prompt still resumes; Shift-Enter still does not.
- The placeholder is drawn in the muted tone, only with the offer up
  and the prompt empty, and never in a focus view or `--view`.
- Tests cover the header's new text, the placeholder's appearance and
  disappearance, and the typed-character dismissal.

Size: S. Source: user request 2026-09-16.
