# Document OpenAI OAuth setup

## Summary

Add a complete README guide for authenticating ilar with a ChatGPT account.

## Requirements

- Document the login command and browser callback flow.
- Document the required OpenAI provider configuration.
- Explain token storage and the state-directory override.
- Explain ChatGPT-compatible model naming and API-key fallback.

## Acceptance Criteria

- A new user can configure OAuth without reading source or example comments.
- The documented commands, paths, and supported values match the implementation.

## Notes

- Added a dedicated README procedure linked from the OpenAI `auth` setting.
- Documented browser fallback, callback address and timeout, sandbox loopback
  requirement, provider config, compatible model selection, token storage,
  automatic refresh, and API-key fallback.
