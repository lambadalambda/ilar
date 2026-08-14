# z.ai GLM provider (Anthropic-compatible + OpenAI-compatible)

## Summary

Second provider: z.ai GLM models. Both endpoint flavors, selected by
config (`flavor = "anthropic" | "openai"`).

## Requirements

- Anthropic flavor: `/v1/messages` SSE stream, content_block deltas,
  tool_use blocks, `x-api-key` + `anthropic-version` headers.
- OpenAI flavor: Chat Completions wire format at z.ai's paas v4 base URL.
- Same neutral-event translation contract as the OpenAI provider.
- API key from config or `ILAR_ZAI_API_KEY`.
- TDD with fixture SSE streams for both flavors.

## Acceptance Criteria

- Both flavors pass fixture tests.
- Tool calls round-trip: neutral tool_call -> wire request, wire stream ->
  neutral events.

## Notes

- z.ai's Anthropic endpoint is close but not identical to Anthropic's
  (GLM-specific fields); tolerate unknown fields everywhere.

## Milestone

1 — Lean core
