# Memory follow-ups from the component review

## Summary

Turn the [2026-11-07 assessment](../memory-review-2026-11-07.md) into focused work. Keep the file-based memory tiers. Fix write safety first, then add source-backed correction and measure whether it helps later tasks.

## Requirements

- Keep bug fixes separate from new retrieval, metadata and maintenance behavior.
- Use regression tests for confirmed failures. Record design decisions before changing access scope or scheduling.
- Do not enable automatic recall or add a wiki or vector database as part of this work by assumption.

## Work order

### 1. Fix write safety

- [Serialize shared memory writes](serialize-shared-memory-writes.md).
- [Memory off stops review writes](memory-off-stops-review-writes.md).
- [Group handovers stay out of personal memory](group-handovers-stay-out-of-personal-memory.md).

These existing issues are independent and can be implemented separately.

### 2. Make source checking possible

- [Memory notes keep their evidence](memory-notes-keep-their-evidence.md).
- [History can show the evidence it finds](history-can-show-the-evidence-it-finds.md).
- [Agents can read scoped source transcripts](agents-can-read-scoped-source-transcripts.md).

Metadata and evidence retrieval can be designed in parallel. A source reference must remain usable after compaction without granting access to unrelated sessions.

### 3. Correct memories within a budget

- [Consolidation checks claims against sources](consolidation-checks-claims-against-sources.md), using the source metadata and retrieval work above.
- [Terminal stores get consolidated](terminal-stores-get-consolidated.md), reusing the correction behavior rather than copying the gateway scheduler.

### 4. Measure usefulness before changing defaults

- [Recall is measured before it is on](recall-is-measured-before-it-is-on.md): repair historical cases and runtime parity before treating selector scores as rollout evidence. This can start alongside the other phases.
- [Memory is measured by later task results](memory-is-measured-by-later-task-results.md): compare task outcomes, correction errors and cost. Use this to test maintenance and shorter memory instructions, not to assume either is better.

## Acceptance Criteria

- Each linked issue has a bounded scope and observable completion criteria.
- The bug fixes are verified before new automatic maintenance is enabled.
- Source access and legacy-note behavior are documented and tested.
- Outcome measurements and their limits are recorded before recall or maintenance defaults change.

## Notes

- Source: https://www.anthropic.com/research/components-of-agent-memory
- This index does not require resolving every lower-priority observation in the assessment. Unicode search behavior and the unconfirmed review-counter race remain separate follow-up candidates.
