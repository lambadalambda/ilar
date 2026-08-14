# Core tools: read, write, edit, bash, glob, grep

## Summary

The six workhorse tools, each declaring `ToolKind::ReadOnly` or
`ToolKind::Mutating`.

## Requirements

- `trait Tool`: name, description, JSON schema (hand-written serde_json),
  `kind()`, `async run(input, ctx) -> ToolOutput`.
- Tool input validation via serde-serialized input structs.
- read: file read with line numbers, size cap, image passthrough later.
- write: create/overwrite file, parent dirs.
- edit: exact-match replace, error on ambiguous/multiple matches.
- bash: async process, streaming not needed v1 (capture output), timeout
  + env/cwd from ctx, `run_in_background` NOT in v1.
- glob / grep: use `walkdir`/`ignore` + `regex` crates (add to deps).
- Tool output: text (markdown-ish) with truncation at a sane cap.
- Each tool TDD'd with tmpdir fixtures.

## Acceptance Criteria

- All six tools have passing unit tests incl. edge cases (missing file,
  ambiguous edit match, bash non-zero exit).
- Tools marked: read/glob/grep = ReadOnly; write/edit/bash = Mutating.

## Notes

- No permission checks anywhere (sandbox handles it) — resist the urge.

## Milestone

1 — Lean core
