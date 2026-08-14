# OpenAI ChatGPT OAuth login (PKCE)

## Summary

`ilar login`: browser-based ChatGPT OAuth so the OpenAI provider works
without an API key (the same flow the Codex CLI uses). Tokens stored
locally, auto-refreshed on 401.

## Requirements

- PKCE (S256) per RFC 7636; authorize URL at auth.openai.com with the
  public client id, scope incl. offline_access.
- Local callback server on 127.0.0.1:1455 (/auth/callback), state
  validated, code+verifier exchanged at the token endpoint.
- TokenSet persisted to `<state dir>/auth.json` (ILAR_STATE_DIR aware).
- chatgpt-account-id extracted from the id_token JWT claims.
- Provider: `auth = "chatgpt"` mode — base URL
  chatgpt.com/backend-api/codex, originator/beta headers, bearer token,
  `store: false`, one refresh-and-retry on 401.
- TUI `ilar login` subcommand: opens the browser (macOS `open`),
  prints the URL, 5-minute callback timeout.

## Acceptance Criteria

- Unit tests: RFC 7636 PKCE vector, JWT claim extraction, callback
  parsing, auth store round-trip, refresh-on-401 retry against mock
  token + responses servers.
- Live: `ilar login` completes, provider answers through the ChatGPT
  backend.

## Notes

- Refresh may rotate the refresh token; ilar keeps its own token file
  (never reads/writes ~/.codex).

## Milestone

3 — Polish & extras (follow-up)
