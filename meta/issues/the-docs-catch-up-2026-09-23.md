# The docs catch up, 2026-09-23

## Summary

Docs, examples and help that drifted from the code.

## Requirements

- A provider key in a sealed store reads "has no credential: set
  ILAR_ZAI_API_KEY" (config/toml.rs:1242 drops the store error): say
  it is stored but sealed, and how to run anyway.
- `ilar.toml.example`: `home = "~/…"` makes a literal `~` directory
  (config.rs:15; the default follows ILAR_STATE_DIR); the agent example
  names `zai/glm-4.7-air`, which the catalog lacks; `addr = …;
  password = …` is not TOML; missing keys `group_mention_only`,
  `general.memory*`, `replay_thinking`, `agent.sudo`,
  `agent.max_output_tokens`, `[endpoints.*]`.
- Agent frontmatter is TOML only (toml.rs:1922); docs imply YAML.
- `ilar --help` does not wrap (no clap `wrap_help`); `--view`'s text
  is four lines.
- `/context` promises 32k…1M but takes any integer; `k` is 1024
  (decide.rs:309-320, interface.md:121).
- `--json` event names beyond `turn_done` are undocumented; a failed
  turn under `--json` emits no JSON event.
- docs/gateway.md: "everything lives in gateway.home" (sessions and the
  outbox do not); internal milestone note at :5; version 0.2.0; a
  repeated paragraph; `run` missing from usage; `pip install` blocked
  on current Debian — suggest pipx.
- docs/configuration.md: "in /model" is the gateway's (the TUI has
  F2); ILAR_SERVE_TOKEN and ILAR_SERVE_POLL_MS missing from the env
  table. docs/serve.md: "three files, about 650 lines".
- The no-provider error ends "No provider is configured yet" with no
  next step (toml.rs:1351); EXEC_UNLOCK_HINT offers only decrypting the
  whole store.
- Unknown keys in a project `ilar.toml` stop startup; say so.

## Acceptance Criteria

- Each item fixed or struck with a reason here.

## Notes

- Source: UX sweep 2026-09-23 (docs pass). Size: M, many S.

## Done (2026-09-23)

`fb164e4`, `8cbdaed`, `c679a47`: a key in a sealed store is said to be
sealed away, with the export that runs without the password; no
provider names `ilar --help`; `ilar --help` wraps (clap `wrap_help`)
and `--view` is one line; `exec --json` ends a failed run with an
`error` event and every event type is documented; `/context` says
what it takes (k and M in 1024s). ilar.toml.example: no literal `~`,
valid TOML, the missing keys; the agent example names a model that
exists; gateway.md: what the home holds and what it does not, pipx,
0.3.0, `run`; configuration.md: strict reading, F2, the serve
variables; agents-and-skills.md: TOML frontmatter; serve.md: its real
size.
