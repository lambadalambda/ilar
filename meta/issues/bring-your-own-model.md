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
- Validation errors in the config voice (the PROVIDERS table's
  wording conventions); docs/configuration.md gains the section
  with a llamacpp and an ollama example.

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
