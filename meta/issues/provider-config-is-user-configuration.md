# Provider config is user configuration

## Summary

Found while user-scoping `[models.*]` (2026-08-27): a project
`ilar.toml` can still override `providers.*` — including
`providers.zai.base_url`, which routes requests carrying the
USER'S api_key to an endpoint the repository chose. Strictly worse
than the models hole that was just closed. Probably the same fix
(user-scoped, warn-and-ignore), but a legitimate use might exist
(corporate proxy per project?) — needs the user's call before
changing behavior.

## Requirements

- Decide: user-scope `providers.*` entirely, or scope only the
  dangerous fields (base_url, api_key, auth) and leave the rest.
- Same warning voice and tests as the theme/models precedents.

## Milestone

13 — Guard rails

## Outcome

Scoped `[providers.*]` entirely: the "only the dangerous fields"
option turned out to be the empty set's complement — base_url,
api_key and auth are the section's ONLY fields, and each is an
exfiltration or billing lever (re-route the user's key, substitute
the repository's key, flip OAuth mode). Same mechanism as
`[models]`: a project layer declaring `[providers]` is warned about
in the established voice and ignored wholesale; the user file and
provider env vars resolve as if the project section did not exist.
`declares_models` generalized to `declares_entries` over either
table. Two tests pinning the old convenience were rewritten as
policy tests — notably `project_can_reset_inherited_chatgpt_auth`
became `project_cannot_reset_chatgpt_auth_or_inject_a_key`: a
project deciding which credential a session runs on was the bug
wearing a feature's clothes. Per-project provider tweaks that were
legitimately useful (API-key billing in one repo) still work via
environment variables, which the person launching ilar controls.
docs/configuration.md's layering paragraph — which still described
project `[models]` entries replacing user entries, stale since the
models hardening — now states the user-scoping rule for both
sections. Field-wise provider merging across layers survives in
`merge_file` but only the single user file feeds it now.
