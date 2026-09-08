# Memory that outlives a session

## Summary

ilar remembers within a session — the log outlives the window, history
searches it — and nothing across sessions. An assistant needs both a
small always-present core and a large searchable archive. Research
2026-09-08 (DEVLOG): OpenClaw, Hermes, Awareness Local and Claude
Code converge on the same shape, and the heavy alternatives (temporal
knowledge graphs, LLM-linked note evolution) buy multi-hop recall at a
cost in tokens and latency that is wrong for a personal assistant.

## Design

- **Core, injected, frozen per session.** `MEMORY.md` (the agent's
  notes about its world) and `USER.md` (the user), under the state dir,
  each with a hard cap (Hermes: 2,200 and 1,375 chars). Injected into
  the system prompt once at session start, so the cache prefix stays
  stable; edits land next session. A `memory` tool with add / replace
  / remove; overflow is an error the model resolves by consolidating.
  Never injected in a group chat.
- **Archive, searched, never injected.** One fact per Markdown file
  with frontmatter (type: decision | solution | preference | event |
  task | risk; title; one-line summary; when; links), plus daily notes
  `memory/YYYY-MM-DD.md`. Indexed in SQLite FTS5 (BM25) with optional
  local embeddings and reciprocal rank fusion; no LLM call at
  retrieval. `memory_search` returns an index (~80 tokens an item:
  title, summary, score, age), `memory_get` the full items chosen —
  progressive disclosure. Recency decay in ranking so an old
  well-worded note does not beat yesterday's update. Session logs stay
  searchable through the existing recall scanner.
- **Three write paths.** The tool, explicitly. A flush at compaction:
  the handover summarizer already runs on the warm cache — add "notes
  worth keeping" to its output and write them to the daily note. A
  post-turn review, opt-in, on the warm cache at the end of a turn
  (the `cache_compact` window), staging writes to core files for
  approval when `write_approval` is on.
- **Children start warm.** A subagent's brief includes the project's
  decision and risk cards (Awareness's `awareness_init`), so a child
  does not rediscover what the tree already knows.
- **Later, not now:** promotion of daily notes into the core by a
  scheduled review (OpenClaw's "dreaming") once cron exists; graph
  links between notes; a shared or published memory.

## Requirements

- Core files with caps, tool, injection rule, group gate.
- Archive format, FTS5 index built on demand, two-phase tools.
- The compaction flush.

## Acceptance Criteria

- A fact stored in one session is found by search in another; the
  core is byte-identical across a session's turns; a group chat's
  prompt contains no core memory; a compaction writes a daily note.
