# Gemini 4 Neon on OpenCode Zen

## Summary

Use Gemini 4 Neon in ilar with an OpenCode Zen key. Zen exposes Neon on Google's native API, not on its OpenAI-compatible endpoint.

## Requirements

- Route `opencode/gemini-4-neon` to Zen's Gemini `generateContent` streaming endpoint with the existing Zen credentials and session headers.
- Support streamed text and tool calls, including replay of Gemini thought signatures with tool calls. Do not add Google account sign-in, Interactions, or Go support in this change.

## Acceptance criteria

- Neon is selectable with Zen configured and uses the native endpoint.
- A test covers the tool-call round trip, including signature replay, and a test covers a streamed text reply.
