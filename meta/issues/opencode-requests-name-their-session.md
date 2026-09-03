# OpenCode requests name their session

## Summary

OpenCode mailed (2026-09-03) that requests to Go missing an
`x-opencode-session` header "may error" from 09/06. opencode itself
sends `x-opencode-session: <session id>`, `x-opencode-request`,
`x-opencode-client` and a `User-Agent` on every gateway request
(`packages/opencode/src/session/llm/request.ts`); the gateway logs them
as metrics and keys affinity on the session. ilar sends none, and
reqwest sends no User-Agent at all, so ilar's traffic is anonymous to
them and about to be refused.

## Requirements

- Both OpenCode wires send `x-opencode-session` carrying the request's
  `cache_key` (the conversation identity every other affinity header
  already uses), `x-opencode-client: ilar`, and
  `User-Agent: ilar/<version>`.
- No other provider's headers change: the Codex backend keeps its
  `session-id`/`thread-id` pair, the public OpenAI API and z.ai keep
  sending nothing extra.

## Acceptance Criteria

- Wire tests: an `opencode/glm-5.2` request and an
  `opencode-go/gpt-5.6-luna` request both carry the three headers with
  the session equal to the request's `cache_key`; a z.ai request does
  not carry them.
- Live smoke still passes on both wires.

## Outcome (2026-09-03)

An `Affinity` policy in transport.rs now owns the conversation headers
for every wire: `None` (public OpenAI, z.ai, custom), `Codex`
(`session-id`/`thread-id`, unchanged) and `OpenCode`
(`x-opencode-session`, `x-opencode-client: ilar`,
`User-Agent: ilar/<version>`). Both OpenCode wires carry it; a request
outside any session (topic naming) names a per-process session so
nothing goes out anonymous. Wire-tested on both wires and on z.ai's
absence; live smoke green on tenco. The mail itself was about the
Python OpenAI SDK, not ilar — ilar's requests had no user agent at all.
