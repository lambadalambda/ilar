# Tasks lists without stalling

## Summary

`TasksTool::run` does up to 20 full session-log loads inline on the
async runtime (subagent.rs:2913-2977) — serve spawn_blocks the same
loads for a reason. Big children stall the whole provider step
behind a "cheap read-only listing".

Size: S. Source: sweep 2026-08-29, subagent.
