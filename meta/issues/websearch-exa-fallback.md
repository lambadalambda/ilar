# Keyless Exa MCP websearch fallback

Sub-issue of [web-tools](web-tools.md).

## Summary

`websearch` currently only registers when `ILAR_TAVILY_API_KEY` is set, so
out of the box the tool is silently missing. Add an Exa backend that calls
the hosted Exa MCP endpoint (`https://mcp.exa.ai/mcp`), which currently
allows keyless, rate-limited access. Use it as the default backend so
websearch works OOB; keep Tavily as the preferred keyed backend.

## Requirements

- `ExaBackend` implementing `SearchBackend`: JSON-RPC 2.0 `tools/call` of
  `web_search_exa` POSTed to the MCP endpoint, `Accept: application/json,
  text/event-stream`.
- Parse both response framings: direct JSON body and SSE (`data: ` lines).
- Parse the text payload (blocks separated by `---` with `Title:` / `URL:`
  lines) into `SearchHit`s; if block parsing yields nothing, fall back to a
  single hit carrying the raw text so results are never dropped.
- Optional `ILAR_EXA_API_KEY` env var, passed as the `exaApiKey` query
  parameter (same mechanism opencode uses).
- Backend selection in `with_web_tools()`: Tavily when
  `ILAR_TAVILY_API_KEY` is set, otherwise Exa. Websearch is therefore
  always registered.
- Reuse the hardened HTTP client (no redirects for the API call, bounded
  body, total timeout) like `TavilyBackend`.
- Document in README: works keyless by default via Exa, but users should
  really configure their own key (Tavily or Exa) — keyless access is
  best-effort and rate-limited.

## Acceptance Criteria

- Unit tests against a local mock server cover: SSE framing, direct JSON
  framing, block parsing into hits, raw-text fallback, oversized response
  rejection, timeout.
- `ToolRegistry::builtin().with_web_tools()` registers `websearch` with no
  env vars set.
- README documents the default and the recommendation to bring a key.

## Notes

- Keyless access depends on Exa's goodwill; degrade with a clear tool
  error if the endpoint starts refusing (HTTP error surfaces as
  `websearch: exa HTTP <status>`).
- Do not follow redirects on the API request (avoid leaking the query/key
  to redirect targets), mirroring the Tavily backend.
