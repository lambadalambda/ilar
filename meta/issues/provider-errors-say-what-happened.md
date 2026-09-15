# Provider errors say what happened

## Summary

- 401/403 is a raw JSON dump: `error: HTTP 401 Unauthorized:
  {"error":{"code":"1002",…}}` (transport.rs:199 → turn.rs:2216 →
  app.rs:1593), body up to 64 KiB, nothing saying the key was
  rejected or where it came from (TOML, env, store; toml.rs:1003).
  A one-line lead ("zai rejected the API key (HTTP 401): check
  ILAR_ZAI_API_KEY / providers.zai.api_key") with the body beneath.
- Connection failures hide their cause: `request_error`
  (transport.rs:150-156) uses Display though the file's own comment
  says Display hides the cause and `error_with_sources` exists.
  Network down reads "retrying in 0.5s (1/3): error sending request
  for url (…)" three times, then the same line bare, with no dns /
  refused / timed out and no "gave up after 3 retries".

Size: S. Source: UX sweep 2026-09-15, first run.
