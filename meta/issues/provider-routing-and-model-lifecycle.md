# Provider routing and model lifecycle

## Summary

Provider objects are selected independently from persisted effective models, breaking resume, model switching, subagents, and compaction across providers.

## Requirements

- Resolve a provider from the effective model for every root, resumed, compacted, and subagent turn.
- Make resumed sessions default to their persisted model and agent.
- Make explicit CLI model selection override agent and general defaults.
- Inherit the parent model for subagents without a model override.
- Reject provider/model prefix mismatches.

## Acceptance Criteria

- Cross-provider resume and model switching use matching providers.
- Subagent overrides resolve their own provider and default children inherit the parent model.
- Compaction uses the effective model.
- Model changes are applied only after successful persistence.
