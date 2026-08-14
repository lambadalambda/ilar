# Concurrency-barrier tool executor

## Summary

The scheduler that runs one turn's tool calls: read-only tools
concurrently, mutating tools as barriers. Port of Claude Code's
`isConcurrencySafe` model to tokio.

## Requirements

- Input: ordered list of tool calls from one assistant turn.
- Scheduling rule: a queued tool may start iff no tool is executing OR
  it and every executing tool are ReadOnly. Mutating tool = barrier
  (wait for all prior, run alone; subsequent tools wait for it).
- Execution concurrent, results collected in **call order** (deterministic
  transcript regardless of completion order) — Claude Code drains in order.
- Esc/abort: cancel running futures best-effort (CancellationToken),
  unstarted calls marked cancelled.
- TDD with mock tools that sleep controlled amounts; assert concurrency
  groups via ordering probes.

## Acceptance Criteria

- Test: 3 read-only tools overlap (verified by timing/instrumentation).
- Test: read-only + edit + read-only never overlaps the edit.
- Test: results in call order even when completion order differs.
- Abort test: cancellation stops pending + running calls.

## Notes

- This is the piece opencode skips; it's cheap here and prevents Edit/Bash
  races. Keep the executor independent of the session store (returns
  results, caller persists).

## Milestone

1 — Lean core
