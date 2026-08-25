# Single-source the catalogs

## Summary

Capability and registration data is scattered across hand-synced
tables:

- model.rs: `supports_vision` is a hardcoded id list, `variants()`
  chains prefix-parsing heuristics (`gpt-5.10` would parse as
  version 10 and silently inherit 5.2 variants), and
  PRICING/CATALOG are independent arrays — capability flags belong
  on `ModelInfo` rows.
- toml.rs: adding a provider means editing four sites
  (resolved-provider construction, `provider_for`,
  `available_models`, `validate_file`) plus `ModelAccess` and
  `fallback_context_limit`; the per-field option-merge boilerplate
  repeats five times.
- tools/mod.rs: adding an optional tool needs a bespoke `with_*`
  constructor plus the hand-maintained `child_tool_names()` list,
  drift caught only by a test.

## Requirements

- Vision/variant/reasoning capabilities move onto catalog entries.
- Provider resolution/validation driven by one table.
- Tool registration derives `child_tool_names` from the registry
  itself.

## Acceptance Criteria

- Adding a hypothetical model/provider/tool in a test touches one
  data site each; existing config and tools tests pass.

## Milestone

12 — Health sweep

## Outcome

Capabilities live on catalog rows: `ModelInfo` gained `vision` and
`variants` fields via const builders, killing the id-pattern
heuristics (a hypothetical gpt-5.10 now gets what its row says, not
what a version parse guesses). Equivalence proven by dumping all 45
rows old-vs-new: empty diff. toml.rs providers collapsed into one
two-row `PROVIDERS` table driving resolution, reach, validation and
fallbacks, with validation wording pinned byte-identical by
exact-equality tests; the option-merge boilerplate became one
macro. tools/mod.rs derives `child_tool_names` from the builtin
registry plus a `ChildTool` table, with a debug assertion tying
published names to `Tool::name()`. One-site tests pin each: a new
model/provider/child tool is one row apiece. (Line counts grew —
the win is one data site, not fewer lines.) (commit
'refactor(catalog): capabilities live on rows, providers in one
table')
