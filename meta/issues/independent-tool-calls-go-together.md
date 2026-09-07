# Independent tool calls go together

## Summary

Every provider request re-reads the whole context, so a turn's cost
scales with its request count, not its tool count. Over 2026-09-05..07
the build agent averaged 1.08 tool calls per request across 8,420
requests (83% of all tool-calling requests carried exactly one call),
while the explore agent, whose one-line prompt mentions parallel
inspection, averaged 3.44. About 1,500 of the build agent's requests
were a lone `read`, `glob` or `grep`. The executor already runs
read-only calls concurrently and the Responses body leaves parallel
tool calls enabled; the base prompt simply never asks for it.

## Requirements

- The base prompt tells the model to issue independent tool calls in
  one response and says why (each response re-reads the context).
- The wording is one sentence; the base prompt stays terse.

## Acceptance Criteria

- A test pins the sentence in the assembled prompt.
- Calls per request for the build agent visibly rises in the next
  sessions (measured from the session logs; no automated gate).

## Notes

- Pruning stale tool results mid-window was considered alongside this
  and decided against: it drops information the model may still need,
  and the win was ~20% of cached reads under optimistic assumptions.
