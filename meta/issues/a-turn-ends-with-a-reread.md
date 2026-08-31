# A turn ends with a reread

## Summary

Every completed turn sets `recheck_pending_question`, and the top of
the loop answers it with `store.load(session_id)` on the UI task
(main.rs:2487-2490). With a warm replay checkpoint that is a tail
read; on any stamp mismatch it is a full canonical parse — a
multi-second hitch at the exact moment the user expects the prompt
back. The turn already knows whether it ended on a question: carry
the answer on `TurnCompletion` instead of re-reading the log to
rediscover it.

Size: S. Source: sweep 2026-08-31, responsiveness & memory.

## Outcome (2026-08-31)

The turn task reads `stranded_question` at its own tail — the writer
just kept the replay checkpoint warm — and carries the answer on
`TurnCompletion::Root`/`::Compaction`. The join assigns latest-wins,
the loop top consumes without a load, and review confirmed the
design self-corrects: every path that can change the truth runs in
the turn slot and refreshes the report itself. Only a panicked task
still pays a loop-top load, best-effort instead of the old `?` that
hard-exited the TUI on an IO hiccup. Pre-existing and unchanged: an
Esc-abort while a question waits can ask the user to answer twice
(the first reply lands in a dead channel).
