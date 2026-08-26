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
