# Recall that picks by meaning

## Summary

Index for bringing ilar's memory recall up to Claude Code's. ilar's
recall ranked notes with BM25 over title, summary and body, and was
turned off on 2026-10-02 (about 23% precision; matches followed the
session's topic, not the message). Claude Code instead lets a small
model pick from one line per memory, told to match what the message is
about (analysis: Obsidian `opencode/knowledge/claude-code-auto-memory.md`,
still true for v2.1.314).

Probed 2026-11-05 with 85 real notes: `openai/gpt-6-luna` through the
ChatGPT login picks sensibly in 2.1–3.4 s and returns nothing for an
unrelated message; DeepSeek 4.1 Flash on Go works but invents ids and is
slower. The Decisions API (`/v1/decisions`, gpt-6-luna) would fit but is
404 on the ChatGPT backend, Zen and Go.

## Sub-issues

- [The opening index lists every note](the-opening-index-lists-every-note.md)
- [A model picks what recall surfaces](a-model-picks-what-recall-surfaces.md)
- [Recall is measured before it is on](recall-is-measured-before-it-is-on.md)
- [Note bodies say why and how to apply](note-bodies-say-why-and-how-to-apply.md) — later
- [Terminal stores get consolidated](terminal-stores-get-consolidated.md) — later
