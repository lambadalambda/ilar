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
