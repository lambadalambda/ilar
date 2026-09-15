# Startup errors name the next step

## Summary

The first minutes on a fresh box end in messages that misdiagnose or
stop short:

- A malformed or unknown-provider `--model` is reported as a missing
  key: `provider_for` swallows `resolve_model`'s error (toml.rs:865),
  so `ilar --model glm-4.7` or `--model anthropic/claude` prints
  "no provider configured for glm-4.7 (set ILAR_ZAI_API_KEY,
  ILAR_OPENAI_API_KEY or ILAR_OPENCODE_API_KEY)" (runtime.rs:397-402)
  with "Caused by: no configured provider for model" beneath, the
  same fact twice. Three messages: not `provider/model-id`; unknown
  provider (list them); provider known but unkeyed.
- The README quick start does not start (README.md:96-97): after
  `ilar login` or exporting `ILAR_OPENAI_API_KEY` as told, the
  default model is `zai/glm-4.7` (toml.rs:749) and `ilar` dies with
  the line above, which names the variable already set and never
  mentions `general.model` or `auth = "chatgpt"`. The no-provider
  line should say which provider the model needs and that another
  is keyed; `ilar login` should end with the TOML lines to add.
- `general.reasoning` is validated against `general.model` only
  (toml.rs:755) but applied to every model (runtime.rs:157,
  317-318): a valid default plus `--model openai/gpt-5.6` or an
  agent with its own `model:` exits with "invalid reasoning for
  openai/gpt-5.6". Drop the variant with a transcript line instead.
- An unknown model id is fatal only when reasoning is set; without
  it the client is built anyway, the meter shows the 200k fallback
  silently (toml.rs:344, 958-962) and the first turn ends in a raw
  "HTTP 400 Bad Request: {…}". Check against `available_models()`
  at resolve.
- `unknown agent "explorer"` (runtime.rs:298) lists no known agents;
  "no sessions to continue (session directory is empty)"
  (main.rs:1016, 1229) also prints when the directory holds only
  child or unreadable sessions.
- `ilar --help` has no after-help naming `ILAR_CONFIG_DIR`,
  `ILAR_STATE_DIR` or the key variables, so a fresh box gets no
  next step from it.

Size: S-M. Source: UX sweep 2026-09-15, first run.
