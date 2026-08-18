# Stabilize OpenAI prompt caching

## Summary

Investigate and fix OpenAI cached-input counts alternating between large hits and
zero so ilar preserves stable request prefixes, reports backend usage accurately,
and supplies supported cache-affinity controls.

## Requirements

- Correlate the latest session's per-request usage with serialized OpenAI request
  prefixes.
- Verify that prior messages, instructions, tools, model, and reasoning options
  remain byte-stable across ordinary turns.
- Distinguish a backend cache miss from absent or differently shaped usage data.
- Verify current OpenAI and ChatGPT backend cache-key support with controlled live
  requests using the configured authentication.
- Add only cache controls accepted by the relevant endpoint.
- Make the TUI's cache display semantics explicit and accurate.

## Acceptance Criteria

- Consecutive eligible requests have a tested stable prefix and stable session
  cache key.
- Cached token usage is parsed from every observed supported response shape.
- Live probes explain the observed zero/high cache pattern or identify a backend
  limitation with captured non-secret diagnostics.
- Cache display no longer implies a cumulative value when showing one request.
- Workspace tests, formatting, and clippy pass.

## Notes

- Never log prompt content, access tokens, authorization headers, or raw encrypted
  reasoning while diagnosing cache behavior.
- Compaction, model changes, and prompt/tool-definition changes are legitimate
  cache-prefix boundaries.
- The affected session kept one model and no compaction boundary. Its normalized
  usage showed real alternating misses and hits, including large hits followed by
  zero-cache requests at larger unchanged prefixes.
- A metadata-only live ChatGPT probe on 2026-08-19 sent three byte-identical,
  cache-eligible requests five seconds apart. Automatic caching reported 0, 6912,
  then 0 cached tokens. Two keyed samples accepted `prompt_cache_key` and reported
  0/0/0 and 0/6912/0; neither demonstrated improved routing. ChatGPT OAuth
  therefore remains on its documented automatic caching behavior.
- Current OpenAI API documentation supports `prompt_cache_key` on the canonical
  Responses endpoint, which receives stable session affinity. No API-key
  credential was configured for a live API probe, and custom gateways omit the
  field by default to preserve compatibility.
