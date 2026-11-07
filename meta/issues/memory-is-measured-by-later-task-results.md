# Memory is measured by later task results

## Summary

Selector agreement measures which notes a model chooses, not whether memory helps an agent finish later work. Consolidation can improve wording while preserving errors or introducing new ones.

## Requirements

- Build a small reproducible multi-session evaluation with ground truth that is independent of the agent's own claims. Include preferences, tested workflows, stale facts and conflicting evidence, plus tasks where no memory helps.
- Compare the same later tasks with no persistent memory, persistent memory without background maintenance, current maintenance and source-backed consolidation. Use identical starting notes for the persistent-memory conditions, reset stores between runs and keep model/tool settings comparable.
- Measure task success, repeated corrected mistakes, false corrections, unsupported claims, token cost, latency and archive growth. Record model versions, budgets and uncertainty from repeated runs.
- Include adversarial stored text and a check that recalled claims do not become instructions or claims of work performed in the current session.
- Use a fixed evaluation set not used to tune prompts. Private real-session cases, if used with permission, stay outside the repository; synthetic fixtures can be committed.
- Run a separate controlled comparison of current memory guidance and shorter purpose-led instructions on representative models. Do not combine a prompt change with a retrieval or consolidation change in the same comparison.
- Write results and limits in DEVLOG. Define rollout criteria before changing recall or maintenance defaults; do not require that a new approach win to complete the measurement.

## Acceptance Criteria

- A documented command reruns the evaluation from clean fixtures without reading the developer's real memory.
- Case-level results report any improvements, regressions or unchanged outcomes, including the no-help and stale-memory cases.
- Human checks confirm a sample of outcome judgments; selector labels alone do not determine task success.
- The recorded comparison reports costs and errors, not only an aggregate score, and gives an explicit rollout or no-rollout decision.

## Notes

- Part of [the memory follow-ups](memory-follow-ups-from-the-component-review.md).
- Reuse case handling from [the recall evaluation](recall-is-measured-before-it-is-on.md) where it fits, but keep retrieval relevance distinct from downstream success.
- The baseline harness can start before consolidation is implemented; add that comparison when the new path exists.
