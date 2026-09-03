# Qwen and MiniMax answer on the chat wire

## Summary

The OpenCode docs place Qwen (both gateways) and MiniMax (Go) on the
Anthropic Messages endpoint, which is why they were left out of
[OpenCode Go and Zen providers](opencode-go-and-zen-providers.md). Live
probes showed every one of them answering on `/chat/completions` too:
the docs column is the SDK opencode uses, not the gateway's only route.
The user wants them, without any Messages-wire work.

## Requirements

- Catalog and price the Qwen and MiniMax rows the docs list, on the
  chat wire, skipping ids past their published deprecation date.
- One live tool-calling turn each for a Qwen and a MiniMax row in the
  smoke test.

## Acceptance Criteria

- `opencode-go/qwen3.8-max` and `opencode-go/minimax-m3` are listed for
  a keyed Go config and route to `/chat/completions`.
- docs/configuration.md no longer says they are not offered.
