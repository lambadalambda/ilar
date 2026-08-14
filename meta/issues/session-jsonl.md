# Session model + JSONL persistence + resume

## Summary

Define the canonical session data model and an append-only JSONL store.
Sessions are the spine of everything: the agent loop appends, providers
read, subagents get child sessions, resume replays.

## Requirements

- `Session` = ordered event log. Events include: user message, assistant
  message (with tool calls), tool result, compaction marker, session meta
  (model, agent, parent session id).
- One JSONL file per session under `~/.local/state/ilar/sessions/<id>.jsonl`.
- Append-only writes; each line one self-describing event (serde
  tag/content or `type` field).
- `Session::load(id)` replays a file into the in-memory model.
- `Session::transcript()` renders the event log into provider-neutral
  message format for API calls.
- Child sessions reference parent id (needed by the Task tool later).
- TDD: round-trip append/load tests, malformed-line handling (skip +
  warn, don't crash), concurrent append not required (single writer).

## Acceptance Criteria

- Unit tests: write N events, reload, identical model.
- Corrupt trailing line tolerated on load.
- `transcript()` maps events to a neutral `ChatMessage` shape other
  crates/modules consume.

## Notes

- Neutral message model lives here too (role, content blocks: text,
  tool_call, tool_result) — providers translate to/from their wire format.
- Keep timestamps + token usage on events; compaction needs them.

## Milestone

1 — Lean core
