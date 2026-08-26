# Bring your own model

## Summary

Approved 2026-08-27: an escape hatch for arbitrary
OpenAI-compatible endpoints — llamacpp, ollama, any third party.
Multiple entries, each a `[models.<name>]` config section; ilar
handle `custom/<name>`. The flavor removal left one
chat-completions implementation (the zai provider); this
generalizes it rather than adding a second.

## Requirements

- Config: `[models.<name>]` with `base_url` (required), `model`
  (wire id, defaults to the section name), `api_key` (optional —
  local servers need none; no Authorization header when absent),
  `context` (required — it drives input/compaction math), `output`
  (optional), `vision` (optional, default false), `display_name`
  (optional). Multiple sections; names validated (kebab-ish, no
  slash); a section clashing with a built-in provider name is an
  error.
- Provider: extract the chat-completions core from zai.rs; zai
  keeps its specifics (coding default URL, catalog vision,
  reserved options, `tool_stream`); custom entries omit zai-only
  body fields and use their configured vision flag. Same mapper,
  same SSE handling; usage absent from a server's stream degrades
  to zeros without error.
- Catalog: runtime entries built from config, consulted by the
  same `find()`/capability surface as the static table (owned
  strings may be leaked once at startup if that keeps `ModelInfo`
  unchanged — config lives for the process anyway). No pricing →
  the existing tokens-only display. Listed by the models tool and
  the picker like any model.
- `options` (added 2026-08-27): an arbitrary per-model TOML table
  (`temperature`, `top_p`, …) merged into every request body via
  the existing `merge_options` path; reserved wire keys refused at
  config validation, not at request time. Custom entries only.
- Validation errors in the config voice (the PROVIDERS table's
  wording conventions); docs/configuration.md gains the section
  with a llamacpp and an ollama example, every field's
  default/required status, the options rules, and an explicit list
  of what custom models do not support (variants, pricing).

## Acceptance Criteria

- Wire test: a `custom/<name>` request hits the configured
  base_url with the configured wire model id, no Authorization
  when keyless, no `tool_stream`.
- A config with two custom sections lists both models with their
  contexts; compaction math uses the declared context.
- Round-trip test against a fake chat-completions server
  (integration harness exists) covering text + tool call + a
  stream with no usage frames.

## Milestone

13 — Guard rails

## Outcome

`[models.<name>]` → `custom/<name>` over the extracted
chat-completions core (`provider/chat.rs`, `ChatDialect`): zai
became a ~40-line newtype keeping its quirks, wire proven
byte-identical; custom entries get configured URL/wire-id/vision,
no Authorization when keyless, no tool_stream, and an `options`
table merged into every body with reserved keys refused at config
validation. Runtime catalog rows answer through the same `find()`
surface (strings leaked once at startup — the type every consumer
passes around stays `&'static ModelInfo`); no pricing → tokens-only
display; the models tool shows the endpoint host in the price
slot. Docs cover every field, both worked examples, and what
custom models don't get. Hardened same-day: `[models.*]` is
user-scoped — project declarations are warned about and ignored
wholesale (half-merged entries would pair a project URL with a
user key). The pre-existing `providers.*` project-override hole is
filed separately. Residuals: registry rows outlive deleted config
entries (benign — resolution checks config first); `output` shapes
the input budget, not generation (`max_tokens` remains settable
via options); the test http_server is duplicated across two
integration files.
