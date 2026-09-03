# Delivered means delivered across compaction

## Summary

Reopening a session re-delivers task completions that already
arrived. `delivery::is_delivered` decides "already in the log" by
scanning `SessionReader::events()`, and that reader holds only the
active compaction window (`replay_state` starts at the last
`Compaction`'s base). Every notification delivered *before* the
session's last compaction therefore looks undelivered at the next
open: `outbox::pending` keeps it, the TUI holds it, and the first
message re-delivers the lot. On this machine (2026-09-03): 246 pending
outbox entries across 40 files, 242 of them present as user messages
in their target logs, nearly all before the last compaction — one root
session would get 72 stale completions on reopen, two others 17 each.

The re-delivery compounds: 58 of those entries are addressed to
subagents, so a reopen resumes each finished or aborted subagent once
per stale entry, and each resume mints a fresh "Nested task
completed/failed" note for the root (the `Propagate` path). That is
the flood the user saw — completions "meant for a subagent, already
sent to it", then the subagent aborted (Esc), then the root buried in
task notifications.

## Requirements

- `is_delivered` reads the whole canonical log, not the active window
  — `SessionStore::audit_events` is that walk (recall uses it), or a
  reader method that falls back past the window the way
  `contains_tool_call_id` does.
- `outbox::pending` and `route_notification` both go through it, so
  adoption compacts the stale entries away and the in-process re-check
  agrees.
- A regression test: a delivered notification, then a compaction, then
  `pending` — must return nothing.

## Acceptance Criteria

- Reopening a compacted session holds only notifications whose text
  appears in no user message anywhere in its log.
- The outbox files above compact to their four genuinely undelivered
  entries on the next open of their roots.

## Notes

- Separate design question, not this issue: whether a reopen should
  resume long-finished subagents at all to deliver their late mail,
  and whether one root prompt should carry several held completions
  instead of one turn each. With the dedupe fixed those paths only
  run for genuinely undelivered work, which is rare.
- Evidence: `~/.local/state/ilar/outbox`, compared against
  `~/.local/state/ilar/sessions` by exact text containment in
  `user_message` events, split at each log's last `compaction` event.

## Outcome (2026-09-03)

`delivery::is_delivered` now takes the store and the session id and
reads the whole canonical log through `SessionStore::audit_events`,
so a delivery the session has since compacted away is still a
delivery (a rewound one too — a repeat is the worse failure). All
three callers go through it: `outbox::pending`, `route_notification`
and serve's adoption. Regression tests on both layers: deliver,
compact everything out of the window, and neither `is_delivered` nor
`pending` forgets. The next open of each affected root compacts its
stale outbox file down to what was genuinely never delivered.

The design questions in the notes — resurrecting finished subagents
for late mail, one turn per held completion — are untouched.
