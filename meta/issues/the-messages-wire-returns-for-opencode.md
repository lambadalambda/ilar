# The Messages wire returns for OpenCode

## Summary

be56531 removed the Anthropic Messages wire along with z.ai's flavor of
it. OpenCode's gateways serve a good part of their catalogs on that wire
only: on Zen every Claude model and the Qwen family, on Go MiniMax and
Qwen. Those rows are absent from ilar's catalog until the wire is back.

## Requirements

- A Messages-wire provider (`POST {base}/messages`, SSE), keyed the same
  way as the other OpenCode wires and routed to by `OpenCodeProvider`
  from a third `ModelAccess` wire.
- Catalog and pricing rows for the models the docs place on it.
- Before building: probe whether the gateway also answers for those
  models on `/chat/completions` — if it does, the rows may only need the
  chat wire and this issue shrinks to a catalog change.

## Acceptance Criteria

- `opencode/claude-sonnet-5` and `opencode-go/minimax-m3` stream a turn
  with tool calls through the Messages wire in a wire test.

## Notes

- Follow-up to [OpenCode Go and Zen providers](opencode-go-and-zen-providers.md).
- The removed implementation is in git history (a4c2ddd introduced it)
  and is a fair starting point; the cache_control breakpoints from
  2ad44fb matter for Claude pricing.
