# Task tool: parallel subagents with child sessions

## Summary

Task tool spawning child agent loops (own session, own provider call),
run concurrently via `JoinSet`, results merged into parent transcript in
call order. Barrier model applies (Task = ReadOnly like Claude Code
does, so sibling tasks parallelize).

## Requirements

- Task tool input: subagent_type/description/prompt (+ task_id resume).
- Child session created with parent pointer; child loop is the same
  agent loop, restricted tool set (no Task inside child beyond depth
  cap).
- Concurrency cap from config (`[subagents].max_concurrent`): over cap =
  tool error telling the model not to retry (Claude Code semantics).
- Depth cap enforcement.
- Result: child's final text as tool output.
- TDD with MockProvider: two parallel tasks, caps, depth.

## Acceptance Criteria

- Mock test: 2 tasks in one turn run concurrently, both results in
  transcript in order.
- Cap test: 11th concurrent task errors with guidance.
- Child sessions resumable/inspectable as JSONL files.

## Notes

- Adopt Claude Code trick: sibling context — pass earlier same-turn
  tool uses into child context if cheap; optional.

## Milestone

2 — Multiply
