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
