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

## Outcome (2026-09-03)

Eight rows added on the chat wire: Zen qwen3.6-plus and qwen3.5-plus;
Go minimax-m3, qwen3.8-max, qwen3.8-flash, qwen3.7-max, qwen3.7-plus
and qwen3.6-plus. Left dark on purpose: Zen's qwen3.7-max/plus answer
"not supported", Go's minimax-m2.7 is a persistent 500 on both wires,
and minimax-m2.5 is past its deprecation date. Live smoke on tenco:
qwen3.8-flash and minimax-m3 both complete a tool-calling turn.
MiniMax emits its thinking inline as a `<think>` block in the text —
see [MiniMax thinks out loud](minimax-thinks-out-loud.md).
