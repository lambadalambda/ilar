# Runtime model switching

## Summary

Change model/agent mid-session from the TUI without restart; config
remains the default source.

## Requirements

- Keybind (e.g. Tab / Ctrl-M) opens model picker from configured
  providers/models.
- Loop takes model per-turn from a shared state cell the TUI can write.
- Session events record model changes (audit in JSONL).
- Switch applies from the next provider call, never mid-stream.

## Acceptance Criteria

- Mock test: model state change reflected in next Request; session log
  records it.

## Milestone

3 — Polish & extras
