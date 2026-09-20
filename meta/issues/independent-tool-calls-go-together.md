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

## Outcome (2026-09-20)

The sentence landed in `7c91c80`, pinned by
`the_base_prompt_asks_for_independent_calls_in_one_response`; the issue
was simply never archived. The measurement is done now, over every
session log on this machine — 3,571 of them, 140k assistant responses,
counting `tool_call` blocks per response for the `build` agent.

**The aggregate says nothing happened**, 1.25 calls per request before
and 1.26 after. That is Simpson's paradox: the model mix moved toward
local Qwen builds in the same period, and they batch worst of all
(1.07–1.09). Holding the model fixed, every model with traffic on both
sides of the change improved:

| model | before | after | single-call rows |
| --- | --- | --- | --- |
| openai/gpt-5.6-sol | 1.31 | 1.38 | 87.8% → 87.0% |
| openai/gpt-6-astra | 1.07 | 1.22 | 97.1% → 89.6% |
| opencode-go/muse-spark-1.3 | 1.04 | 1.29 | 97.7% → 77.0% |

The weakest response is from the model that was already batching most.
The two that were issuing one call per request almost every time — 97%
of rows — are the ones that moved, which is what the prompt was for.
`explore`, whose prompt always asked for this, went 3.27 → 3.81 over
the same window, so some of the lift is drift rather than the sentence;
the per-model build numbers are larger than that drift in two of three
cases.

Worth knowing for later: a local model may need this said in the
tool-calling protocol rather than in prose.
