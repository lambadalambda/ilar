# The memory store moves into the core

## Summary

Memory that outlives a session is a gateway feature: the two core
files, the note archive, the BM25 index with recency decay and the
three tools live in `crates/ilar-gateway/src/memory.rs` and nowhere
else. A TUI session has no memory tier at all, and the TUI on the same
machine as a gateway cannot see what the assistant remembered.

Claude Code keeps one memory set per project directory, outside the
repo, keyed by the launch path. ilar's core already keys its state by
the canonical launch directory (the last-session pointer, the project
config layer, AGENTS.md), so the same key serves here. The store,
index and tools move into `crates/ilar`, parameterised by a root
directory; the gateway keeps one root under its home, the TUI gets
one per launch directory.

## Requirements

- `MemoryStore`, the note archive, the search index and the tools
  `memory`, `memory_search` and `memory_get` move to the core crate,
  taking a root path. Behaviour and caps are unchanged: `MEMORY.md`
  (2,200 characters), `USER.md` (1,375), `notes/`, `daily/`.
- The gateway constructs its store at `<home>/memory/` exactly as
  today. Its after-turn review, weekly promotion, handover daily
  notes and the room-seat withholding stay in the gateway crate.
- The TUI's root is `<state dir>/memory/<slug>/`, where the slug is
  derived from the canonical launch directory the last-session
  pointer already computes. Two spellings of one directory share one
  memory; a subdirectory of a checkout is another directory, as it is
  for sessions.
- The core block is injected into the system prompt once at session
  open and frozen for the session, as the gateway does now. Nothing
  about the prefix changes mid-session.
- Subagents get no memory tools; the root session only, like
  `history`.
- No shared crate for the gateway's reviewer: it keeps calling the
  store through the moved API.
- `[general] memory = false` (default `true`) leaves a terminal
  session without the tools and the block; the gateway keeps
  `gateway.memory.enabled`. Pulled forward from the next issue at the
  user's request, 2026-09-18.

## Acceptance Criteria

- Every existing gateway memory test passes unchanged in meaning,
  whether it moves with the code or stays behind.
- A core test opens a store at a per-directory root, writes to it,
  reopens it at the same directory spelled differently and reads the
  same core block in the next plan's prompt.
- A TUI session in a directory with no memory yet has the tools and
  an empty store; nothing is created on disk until something is
  written.
- docs/gateway.md's memory section moves to a core document (or
  docs/sessions.md) with the gateway's additions left as a
  paragraph there; docs/configuration.md names the TUI root.

## Notes

The gateway's reviewer, weekly job and room guard are what make memory
an assistant's; they do not move. What moves is the storage and the
retrieval, which are the same on both surfaces.

Sub-issue 1 of 3 of the memory stream, after the Claude Code memory
write-up in the vault (2026-09-18). The next two are
[the-tui-remembers-on-its-own] and
[recall-comes-to-the-turn].

Size: M. Source: comparison with Claude Code auto-memory, 2026-09-18.
