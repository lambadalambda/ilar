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
