# Provider trait + event model

## Summary

The provider abstraction: one trait, one streaming event enum, neutral
request/response types. Everything downstream (loop, tests, both real
providers) programs against this.

## Requirements

- `trait Provider { async fn stream(&self, req: Request) -> Result<EventStream>; }`
- `Request`: model id, system prompt, transcript (neutral messages),
  tool definitions, temperature/etc passthrough.
- `ProviderEvent` enum: `TextDelta`, `ToolCallStarted` (id, name),
  `ToolCallInputDelta`, `ToolCallCompleted` (parsed args), `TurnComplete`
  (stop reason, usage), `Error`.
- Event stream is `impl Stream<Item = ProviderEvent>` (or channel-based).
- A `MockProvider` yielding scripted events, for TDD of the loop and
  executor.
- Provider resolution from config string `"provider/model-id"`.

## Acceptance Criteria

- MockProvider drives a consumer test end-to-end.
- Trait shape compiles against both OpenAI Responses and Anthropic-style
  streaming semantics (checked on paper in review).

## Notes

- No retries/backoff in this issue — separate concern, later.
- Cancellation: dropping the stream must abort the underlying request
  (abort handle in the impl).

## Milestone

1 — Lean core
