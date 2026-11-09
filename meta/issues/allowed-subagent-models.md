# Allowed subagent models

## Summary

Let the user restrict which models subagents can use, with controls in the model picker.

## Requirements

- Default to only the parent's current model, including after a model switch.
- Let the user mark allowed models in the model picker and restore the default.
- Persist the selection and enforce it in core subagent start and resume paths, not just in tool hints.
- Apply the policy to explicit model overrides, agent defaults, and nested subagents.

## Acceptance criteria

- Tests cover default, explicit allowlist, denied overrides and resumes, and model-picker controls.
- Existing model selection and reasoning controls still work.
- Document configuration and menu controls.
