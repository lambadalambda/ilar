# Session: small frictions

## Summary

Omnibus; tick items off here.

- A refusal raised under the full-screen session search ("input has
  an unsent draft; send or clear it first", main.rs:3914-3924) is
  drawn under the modal (modals.rs:2176-2180 clears the notice row):
  Enter on a row does nothing visible and the draft cannot be
  cleared from inside. Draw it in the modal or refuse at open.
- During a turn Ctrl-P, F2, F3 and Ctrl-X are dead with no feedback
  (app.rs:613-619, main.rs:4289-4315) while the banner says "Ctrl-P
  commands"; items legal mid-turn (Pending, Help, Export, Open link)
  become unreachable. Open and refuse per item, or a notice.
- Esc / Ctrl-C erase a draft, multi-line paste included, with no
  undo (main.rs:4337-4339). Stash a multi-line draft or ask twice.
- `ilar --continue` opens the globally newest session
  (`store.latest()`, main.rs:1223-1233) regardless of directory:
  from `~/repos/b` it opens a's conversation with tools rooted in b.
  Prefer this directory's latest, else say where it started.
- A typed command that fails at the decide layer loses its text
  (main.rs:4419 takes the input before `decide::submit` returns a
  Notice) while the same failures at `prepare_prompt` restore it:
  `/context lots`, `/compact now`, mid-turn `/rewind`.
- Goal prompts carry ten-space runs into the transcript
  (main.rs:752-766 string literals) shown verbatim as `you` rows.
- A big paste grows the input to `height-4` and the transcript to
  one row, and the pending strip truncates to nothing
  (view.rs:580-600). Cap around 40% and scroll inside.
- While "restoring session", Esc and Ctrl-C do nothing, not even
  clear a draft (main.rs:1381-1385, 4330-4339).
- The terminal title stays "ilar — <topic>" after quit
  (`apply_terminal_title` only at 2834, 3335).
- `/fork` copies the Topic event (store.rs:534-537) and titling runs
  only when `topic.is_none()`, so both sessions wear one name for
  ever. Drop the Topic on fork or suffix it.
- Pending manager: Enter on "background jobs: N running" and
  "services: N running" does nothing (app.rs:1788-1793) while the
  footer promises "Enter edit/act".

Size: S. Source: UX sweep 2026-09-15, session lifecycle.
