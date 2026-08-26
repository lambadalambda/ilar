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
| `models.<name>.*` | — | Your own OpenAI-compatible endpoint, used as `custom/<name>`. See [Bring your own model](#bring-your-own-model). |
| `agent.max_iterations` | `1000` | Max provider calls per user turn (runaway-loop backstop). |
| `compaction.threshold` | `0.85` | Context fraction at which history is handed over; must be between 0 and 1. |
| `subagents.max_concurrent` | `10` | Maximum concurrent subagents; must be at least 1. |
| `subagents.max_depth` | `3` | Maximum nested subagent depth; must be at least 1. |
| `subagents.background_tool_timeout_ms` | `600000` | Background tool timeout in milliseconds; must be at least 1. |

Environment variables:

| Variable | Purpose |
| --- | --- |
| `ILAR_CONFIG_DIR` | Replaces the default `~/.config/ilar` user configuration directory. |
| `ILAR_STATE_DIR` | Replaces the default `~/.local/state/ilar` session, authentication and spilled-tool-output directory. |
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

## Bring your own model

Any server that speaks the OpenAI `/chat/completions` dialect can be a model in
ilar: llama.cpp, ollama, vLLM, LM Studio, a company gateway, a third-party API.
Each one is a `[models.<name>]` section, and each one becomes the model id
`custom/<name>` — selectable in the picker, usable as an agent's `model`, listed
by the `models` tool.

| Field | Required | Default | Description |
| --- | --- | --- | --- |
| `base_url` | yes | — | Everything up to `/chat/completions`, e.g. `http://127.0.0.1:8080/v1`. Must be an `http://` or `https://` URL with a host. |
| `context` | yes | — | The window the endpoint serves, in tokens. There is no catalog row behind a custom model, so this number is the only thing input budgeting and compaction have; see below. |
| `model` | no | the section name | The id to put on the wire, when the server calls the model something else (`llama3.3:70b`). |
| `api_key` | no | none | Sent as `Authorization: Bearer …`. With no key, **no Authorization header is sent at all** — which is what a local server wants. There is no environment-variable fallback for these entries. |
| `output` | no | a quarter of `context` | Tokens reserved for the reply. A local server's window is one budget shared by prompt and reply, so some of it is held back. |
| `vision` | no | `false` | Whether the model accepts images. When false, images in the session are replaced with `[image omitted: this model cannot view images]` rather than refused. |
| `display_name` | no | the section name | Name shown in the picker and the models tool. |
| `options` | no | none | A table of extra body fields merged into every request to this model. |

`options` is passed through as typed — `temperature = 0.7` arrives as the number
`0.7`, a string stays a string — and is merged into the request body alongside
the fields the wire builds itself. Those fields are reserved: `model`,
`messages`, `tools`, `stream` and `stream_options` cannot appear in `options`,
and a config that names one is refused at startup rather than mid-turn:

```
~/.config/ilar/ilar.toml: models.qwen.options cannot override: model, stream
```

Names are validated too: no slashes (the id already has one), not empty, and not
`openai`, `zai` or `custom` — a section named after a provider would produce an
id nothing could resolve.

### Context and compaction

`context` is what the context meter fills and what compaction triggers against.
The budget is `context - output`, so `context = 32768` with no `output` compacts
against 24576 tokens at `compaction.threshold`. Declaring more than the server
actually serves is the failure mode to avoid: requests are rejected at the
server rather than compacted in time. If the server was started with
`--ctx-size 32768`, write 32768.

### Two worked examples

llama.cpp, keyless, serving one local model with its own sampling defaults:

```toml
[models.qwen]
base_url = "http://127.0.0.1:8080/v1"
context = 32768
display_name = "Qwen3 Coder (local)"
options = { temperature = 0.7, top_p = 0.9, min_p = 0.05 }
```

ollama, whose wire ids carry a tag the section name should not have to:

```toml
[models.llama]
base_url = "http://127.0.0.1:11434/v1"
model = "llama3.3:70b"
context = 128000
output = 8000
display_name = "Llama 3.3 70B"
```

Then select either one:

```toml
[general]
model = "custom/qwen"
```

### What custom models do not get

- **No reasoning variants.** `general.reasoning` and an agent's reasoning input
  are rejected for a `custom/*` model: ilar has no ladder for an endpoint it
  knows nothing about, and the vocabulary is per-model.
- **No pricing.** Usage is reported in tokens; there is no cost line, because
  there is no price list for your own server. The `models` tool prints the
  endpoint's host where a cataloged model prints its rate — `custom/qwen · ctx
  32k · 127.0.0.1:8080`.
- **No cache metrics unless the server reports them.** Cache reads and writes
  come from the `usage` frames a server sends; llama.cpp and friends generally
  send none, and a stream with no usage at all completes normally with a zero
  token count rather than erroring.
- **No `tool_stream`.** That body field is z.ai's and is not sent here.

Project configuration layers by section name: a `[models.<name>]` in
`./ilar.toml` replaces a user entry of that name **outright** rather than
merging into it field by field — an endpoint description is one thing, and a
project's `base_url` inheriting the user's `api_key` is not a thing to send.
Entries the project does not name are inherited untouched.

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
