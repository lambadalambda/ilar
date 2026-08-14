# Auto-compaction

## Summary

When transcript nears the model's context window, summarize older turns
into a compaction marker and continue. Required for real work.

## Requirements

- Token accounting per provider (usage from TurnComplete; estimate via
  chars/4 fallback).
- Threshold from config (`[compaction].threshold`, default 0.85).
- Compaction prompt: keep tasks/decisions/open loops/file paths; produce
  summary written as a compaction event in the JSONL; post-compaction
  transcript = summary + recent tail.
- Compaction call goes through the provider (own small request, not
  part of main transcript); can target a cheaper model later.
- Session remains resumable across compaction boundary.
- TDD with MockProvider scripting a compaction summary.

## Acceptance Criteria

- Mock test: oversized transcript triggers compaction call, session
  continues, JSONL contains marker, reload produces compacted view.

## Notes

- Never compact the last N turns (recency window); summary + tail
  boundaries configurable-ish.

## Milestone

2 — Multiply
