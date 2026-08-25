# Configuration

The user configuration is `${ILAR_CONFIG_DIR:-~/.config/ilar}/ilar.toml`; see
[`ilar.toml.example`](../ilar.toml.example). `./ilar.toml` and
`./.ilar/ilar.toml` layer project settings over it, in that order. Nested
sections merge by field. `general.theme` and `general.project_instructions` are
user-scoped and are not overridden by project files; a project file that sets
one is reported in the transcript at startup rather than silently ignored.

| Setting | Default | Description |
| --- | --- | --- |
| `general.model` | `zai/glm-4.7` | Default `provider/model-id`. |
| `general.reasoning` | provider default | Default reasoning variant for new sessions (for example `low`, `high`, or `max`; model-specific). Set `default` in a higher config layer to clear an inherited value. |
| `general.theme` | `carbon` | See [themes](interface.md#themes). F3 opens the picker. |
| `general.project_instructions` | `true` | Whether the working directory's `AGENTS.md`/`CLAUDE.md` is part of the system prompt. See [Project instructions](#project-instructions). User-scoped. |
| `providers.openai.base_url` | API or ChatGPT endpoint | Override the Responses API base URL selected by `auth`. |
| `providers.openai.api_key` | `ILAR_OPENAI_API_KEY` | OpenAI API key. |
| `providers.openai.auth` | `api_key` | `api_key` or `chatgpt`; see [OpenAI ChatGPT OAuth](#openai-chatgpt-oauth). |
| `providers.zai.base_url` | `https://api.z.ai/api/coding/paas/v4` | Override the z.ai OpenAI-compatible base URL. |
| `providers.zai.api_key` | `ILAR_ZAI_API_KEY` | z.ai API key. |
| `agent.max_iterations` | `1000` | Max provider calls per user turn (runaway-loop backstop). |
| `compaction.threshold` | `0.85` | Context fraction at which history is handed over; must be between 0 and 1. |
| `subagents.max_concurrent` | `10` | Maximum concurrent subagents; must be at least 1. |
| `subagents.max_depth` | `3` | Maximum nested subagent depth; must be at least 1. |
| `subagents.background_tool_timeout_ms` | `600000` | Background tool timeout in milliseconds; must be at least 1. |

Environment variables:

| Variable | Purpose |
| --- | --- |
| `ILAR_CONFIG_DIR` | Replaces the default `~/.config/ilar` user configuration directory. |
| `ILAR_STATE_DIR` | Replaces the default `~/.local/state/ilar` session and authentication directory. |
| `ILAR_OPENAI_API_KEY` | Fallback OpenAI API key. |
| `ILAR_ZAI_API_KEY` | Fallback z.ai API key. |
| `ILAR_TAVILY_API_KEY` | Switches web search to the Tavily API (recommended). |
| `ILAR_EXA_API_KEY` | Authenticates the default Exa web search backend. |

## Web search

The `websearch` tool works out of the box: without any configuration it calls
the hosted [Exa](https://exa.ai) MCP endpoint anonymously. Keyless access is
best-effort and rate-limited by Exa, so for real use you should bring your own
key — either `ILAR_TAVILY_API_KEY` to use Tavily, or `ILAR_EXA_API_KEY` to
authenticate against Exa. If both are set, Tavily wins.

## OpenAI ChatGPT OAuth

ilar can use a ChatGPT account through the same PKCE browser flow as Codex CLI;
an OpenAI API key is not required in this mode.

1. Run the login command:

   ```sh
   ilar login
   ```

2. Complete authorization in the browser. ilar also prints the URL in case the
   browser does not open automatically. The process waits up to five minutes
   for the callback on `http://localhost:1455/auth/callback`, so the external
   sandbox must allow that loopback listener and callback.

3. Select ChatGPT authentication and a compatible model in
   `${ILAR_CONFIG_DIR:-~/.config/ilar}/ilar.toml`:

   ```toml
   [general]
   model = "openai/gpt-5.6-sol"

   [providers.openai]
   auth = "chatgpt"
   ```

   ChatGPT uses its Codex model catalog rather than the standard API-key model
   catalog. `openai/gpt-5.6-sol` is one supported example; the in-app model
   picker lists the models available for the active authentication mode. Leave
   `providers.openai.base_url` unset to use the built-in ChatGPT backend.

Tokens are stored with owner-only permissions in
`${ILAR_STATE_DIR:-~/.local/state/ilar}/auth.json` and refresh automatically.
Treat that file as a credential. To return to API-key authentication, set
`auth = "api_key"` (or remove `auth`) and provide `ILAR_OPENAI_API_KEY` or
`providers.openai.api_key`.

## Project instructions

ilar reads `AGENTS.md`, or `CLAUDE.md` when `AGENTS.md` is absent, from exactly
two locations:

- `${ILAR_CONFIG_DIR:-~/.config/ilar}/`
- The current working directory

When both locations contain instructions, user instructions are included first
and working-directory instructions second. ilar does not search parent
directories or combine instructions from an ancestor tree. See
[System prompts and session context](system-prompts.md) for prompt
composition, refresh timing, subagents, and compaction handovers.

### Skipping the project's file

A project's `AGENTS.md` is unauthenticated third-party input: often a year
stale, occasionally written to steer an agent somewhere you did not ask it to
go. Two knobs leave it out without deleting or editing the file:

- `--no-project-instructions` on `ilar` and `ilar exec`, for one launch.
- `general.project_instructions = false` in your **user** config, to distrust
  project files by default; `--project-instructions` then opts a directory in
  for one launch. A project's own `ilar.toml` cannot set the key — the
  directory under suspicion does not get to vote on whether it is trusted —
  and a project file that tries is reported at startup.

The flags win over configuration in both directions and cannot be combined.
Either way only the "Working directory context" section is dropped: your own
`${ILAR_CONFIG_DIR:-~/.config/ilar}/AGENTS.md` and the base prompt are
unaffected, subagents spawned by the session inherit the refusal, and prompt
assembly never opens the file — it only checks whether it is there.

The escape is prompt-level. It stops the file being handed to the model
unasked; it does not stop the model reading it with a tool, and a session
resumed from a launch that trusted the file may still quote it in transcript
or compaction summaries.

When a project file exists but was skipped, the TUI opens the session with a
system line naming it and the knob responsible:

```
project AGENTS.md present but skipped (--no-project-instructions)
project CLAUDE.md present but skipped (general.project_instructions = false)
```

The decision is made at launch and nothing about it is stored on the session:
the system prompt is rebuilt from configuration, the flags and the working
directory every time. Resuming a session started without the flag, under the
flag, gets a prompt without the project file — and resuming it again without
the flag brings the file back. Escaping a hostile file must not be undone by
`--continue`.
