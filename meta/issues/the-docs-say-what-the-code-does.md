# The docs say what the code does

## Summary

docs/interface.md promises behaviour the code no longer has, and
one refusal message is now false:

- "attachments only ride a fresh turn — while a turn runs, submit
  puts your text back": images send with steers and queued messages
  (main.rs:462-500); only *attaching* mid-turn is refused, and that
  refusal `"a turn is running — images send with a fresh message"`
  (app.rs:1848) is itself no longer true.
- "a session switch … is refused while any stash waits": the stash is
  carried (main.rs:2951, test `a_waiting_stash_does_not_block_a_session_switch`).
- Status-line example shows `out ~8.4k`; the code prints `~8400`
  (view.rs:395) and the `(tasks N)` suffix is undocumented.
- "Switching sessions" promises topic, last words and age per row;
  the excerpt was removed (modals.rs:2284-2286) and the preview pane
  that replaced it exists only at ≥96 columns (2164).
- "The sidebar" says only an undeliverable result claims the notice
  line; a merely *held* one does too (schedule.rs:369-380).
- "grouped tool calls align their columns" holds only at ≥72 columns
  (transcript.rs:2441).

## Requirements

- Fix the doc or the code for each, and say which in the outcome.

Size: S. Source: UX sweep 2026-09-03 (all three).

## Outcome (2026-09-19)

All six settled; five were the doc's fault, one the code's.

- **Attachments.** The doc was wrong: a draft's attachments travel as
  a steer or a queued message, not only on a fresh turn. What a
  running turn refuses is *attaching*, and that refusal said "images
  send with a fresh message", which was false — the code now says to
  attach when the turn ends and that a draft already holding
  attachments still sends them.
- **The stash.** The doc was wrong: a session switch carries the
  stash, as `a_waiting_stash_does_not_block_a_session_switch` pins.
  The sentence now names only the Ctrl-D warning.
- **The status line.** The example showed compact figures the wide
  line does not use: `out ~8.4k` where the code prints `out ~8400`,
  and `Σ 1.2M` where it prints `Σ 1m`. Corrected.
- **The session picker.** The row no longer carries the last words
  said, and the preview pane exists only from 96 columns up. Both
  said now.
- **The notice line.** A held result claims it too, not only an
  undeliverable one.
- **"Grouped tool calls align their columns"** is no longer in
  docs/interface.md; nothing to fix.
