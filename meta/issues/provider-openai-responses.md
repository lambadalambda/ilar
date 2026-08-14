# OpenAI Responses API provider (streaming)

## Summary

First real provider: OpenAI Responses API with SSE streaming and
tool-calling, translating to the neutral event model.

## Requirements

- reqwest + rustls; SSE parse (`event:`/`data:` framing) on the Responses
  stream (`with stream=true`).
- Map Responses events: output text deltas, function/tool call arguments
  (concatenated deltas -> parsed JSON), completed, usage.
- API key from config or `ILAR_OPENAI_API_KEY`.
- Base URL overridable (proxies).
- Errors mapped to `ProviderEvent::Error` / typed errors; drop = abort.
- TDD with recorded/fixture SSE byte streams fed through the parser.

## Acceptance Criteria

- Fixture SSE fixtures (text-only, tool-call, error) parse to expected
  neutral event sequences.
- Manual smoke test against the live API recorded in DEVLOG.

## Notes

- Responses API tool format differs from Chat Completions (nested output
  items, `type: "function_call"`); keep translation in this crate module.

## Milestone

1 — Lean core
