# A session switch replays the world

## Summary

Switching into a session freezes the terminal for the whole restore:
`app.restore_session` (main.rs:1232) → `restore_child_activity`
(session_view.rs:443-475) `store.load`s *every* child with a
`child_session_id`, recursively to depth 8, inline on the UI task —
and children of a crashed run often have no fresh replay checkpoint,
so each is a full JSONL parse. The same stretch then stacks four more
O(log) reads before the first frame: the writer replay (main.rs:1220),
`session_context_tokens` (full load + transcript, main.rs:1347),
`outbox::pending` (per-file parent loads + ancestry walks + flocked
rewrites, main.rs:1284), and `run_app`'s own pending-question load
(main.rs:2468) — the same log read up to three times. Small repo: tens
of ms. A large tree: seconds to tens of seconds with no frame and no
input.

Fix shape: load once and share; build the restored view on
`spawn_blocking` and hand lines back through a channel; adopt the
outbox in the background; lazy-load child timelines on expand.

Size: M-L. Source: sweep 2026-08-31, responsiveness & memory.

## Outcome (2026-08-31)

Three moves. The duplicate reads are gone: the pending-question
answer is taken from the reader the open already loaded
(`initial_pending_question_id`), and run_app's two startup loads were
deleted. The restore — whole-log fold, per-child loads, context
estimate — runs on `spawn_blocking`; the loop draws "restoring
session" and joins the handle mid-loop; `land_restored_view` splices
history where the open stood, ahead of anything pushed meanwhile
(startup notices, a turn started by answering the question under the
modal), and adds landed totals *under* live-accrued usage instead of
resetting it. The outbox adoption scan moved to a worker too; its
results land held, and pause delivery only if the user has not
engaged by then. Review-found and fixed pre-commit: the landing
clobbered a mid-restore turn's usage/cost and could regress an exact
context number to an estimate.

Left on the table, deliberate: lazy child timelines (folds into
[[finished-children-keep-their-transcripts]] — squash + rebuild on
expand is the same design); the landing's SendQueued checks only
turn/question, not the full `queue_step` discipline; `engaged` has a
one-iteration blind spot for a turn that spawns and dies within one
pass (recoverable — the notice's own gesture clears it).
