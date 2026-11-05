# `ilar models` on the CLI

## Summary

An agent that calls `ilar exec --model …` has no way to learn the model
ids short of a model turn that calls the `models` tool. That turn costs
money and fails when the default model is unreachable.

## Requirements

- `ilar models` prints the models this configuration reaches, one per
  line, in the `models` tool's format (same function, so they cannot
  drift): catalog rows, `[models.*]` and `[endpoints.*]` entries.
- `ilar models --json` prints an array of objects for scripts: `id`,
  `name`, `context`, `pricing`, `reasoning` (variant ids).
- No model call, no secret-store prompt (like `exec`).

## Acceptance Criteria

- Unit tests cover the JSON shape and the shared line format.
- `ilar models` runs on a box with a configuration and lists ids that
  `ilar exec --model` accepts.
- Full gate green.
