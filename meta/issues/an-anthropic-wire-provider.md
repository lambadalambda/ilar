# An Anthropic-wire provider

## Summary

ilar speaks OpenAI-shaped wires, ChatGPT OAuth and the OpenCode
gateways; the Zen catalog leaves the Claude family out. picoclaw has a
native Claude provider. Only worth doing if the assistant should run
on Claude through the API.

## Requirements

- Messages API with streaming, tool use, prompt caching breakpoints,
  extended thinking; usage mapped onto the existing `Usage`.
- Catalog rows with prices.

## Acceptance Criteria

- The provider contract tests pass; a live one-token turn answers.
