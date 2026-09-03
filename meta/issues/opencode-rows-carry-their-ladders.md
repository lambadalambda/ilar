# OpenCode rows carry their ladders

## Summary

The OpenCode catalog rows were added with an effort ladder only on the
GPT ids (copied from their `openai/` twins) and none elsewhere, although
models.dev records `reasoning_options` for most of them: Muse Spark and
Grok on the Responses wire, and GLM, Kimi, DeepSeek and Qwen on the chat
wire. opencode itself offers those levels; ilar refuses them. Reported
by the user for Muse Spark 1.3, 2026-09-03.

## Requirements

- Every cataloged OpenCode row carries the `effort` values models.dev
  lists for it, verbatim (the ladders differ per model and must not be
  guessed from the id).
- Responses-wire rows send `reasoning.effort`; chat-wire rows send
  `reasoning_effort`, which is what opencode sends through
  `@ai-sdk/openai-compatible`.
- `toggle` and `budget_tokens` options are out of scope: ilar has no
  thinking on/off switch.
- A twin GPT row may carry more levels than its `openai/` twin when
  models.dev says so; the twin test compares windows, not ladders.

## Acceptance Criteria

- `variant_options("opencode-go/muse-spark-1.3-contributor", Some("xhigh"))`
  yields `reasoning.effort`; `variant_options("opencode-go/glm-5.3",
  Some("max"))` yields `reasoning_effort`.
- Live: `reasoning_effort` changes the reasoning token count on a
  chat-wire row.

## Outcome (2026-09-03)

Every OpenCode row now carries the `effort` values models.dev lists for
it — eight new rung sets, named by their rungs (`EFFORT_NONE_TO_MAX`,
`EFFORT_MINIMAL_TO_XHIGH`, …) since the same rungs recur across vendors.
Responses rows send `reasoning.effort`, chat rows `reasoning_effort`.
Live: on Go glm-5.3 the same prompt spent 23 reasoning tokens at `low`
and 75 at `max`. models.dev also lists `max` on the openai gpt-5.6
rows, so those gained it and the twin test still holds ladders equal.
`toggle` and `budget_tokens` options remain unmodelled.
