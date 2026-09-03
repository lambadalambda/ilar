# OpenCode Go and Zen providers

## Summary

OpenCode sells two gateways: Zen (pay-as-you-go, `https://opencode.ai/zen/v1`)
and Go (a $10/month subscription with usage caps,
`https://opencode.ai/zen/go/v1`). One API key serves both. Each gateway
fronts every model on one of three wires — OpenAI chat-completions, OpenAI
Responses, or Anthropic Messages (plus Gemini's own on Zen). ilar speaks
the first two, so most of both catalogs are one provider row away.

Docs: https://opencode.ai/docs/zen/ and https://opencode.ai/docs/go/.

## Requirements

- Two providers, `opencode` (Zen) and `opencode-go` (Go), named as
  opencode itself names them so a model id reads the same in both tools.
- Both read `ILAR_OPENCODE_API_KEY` (one key upstream, `OPENCODE_API_KEY`),
  or `[providers.opencode].api_key` / `[providers.opencode-go].api_key`,
  with `base_url` overridable as for every other provider.
- Catalog rows for every model the docs place on the chat-completions or
  Responses endpoint, with the wire recorded per row so the provider can
  route a request to `/chat/completions` or `/responses` without asking.
  Models past their published deprecation date are left out.
- Pricing rows from the docs (the ≤-threshold tier where a model has two,
  off-peak for DeepSeek), so the meter shows dollars — on Go that is what
  the usage caps count.
- GPT rows on the Responses wire carry the same windows and effort ladders
  as their `openai/` twins; `reasoning` variants reach the wire as
  `reasoning.effort` there.
- Models on the Anthropic Messages wire (Claude, Qwen on Zen; MiniMax and
  Qwen on Go) and the Gemini wire are out of scope here — see
  [The Messages wire returns for OpenCode](the-messages-wire-returns-for-opencode.md).

## Acceptance Criteria

- `opencode/gpt-5.6-luna` posts to `{base}/responses`;
  `opencode/glm-5.2` posts to `{base}/chat/completions`, with no
  `tool_stream` field (that one is z.ai's).
- A keyed `[providers.opencode-go]` lists exactly the Go rows in the
  catalog; keyless lists none. Same for Zen.
- `variant_options("opencode/gpt-5.6-sol", Some("high"))` produces the
  OpenAI `reasoning.effort` body.
- Every catalog row has a positive input budget (Grok publishes
  output = context; the row must not compute to zero).
- docs/configuration.md, ilar.toml.example and the README name the new
  providers and the key.

## Notes

- Unknown model ids under either prefix go to chat-completions, the wire
  most of the catalog uses.
- Live model lists: `https://opencode.ai/zen/v1/models` and
  `https://opencode.ai/zen/go/v1/models` (unauthenticated).
- Catalog data from models.dev (`opencode`, `opencode-go`) cross-checked
  against the docs' endpoint tables on 2026-09-03.
