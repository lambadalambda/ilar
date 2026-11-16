# Configuration

The user configuration is `${ILAR_CONFIG_DIR:-~/.config/ilar}/ilar.toml`; see
[`ilar.toml.example`](../ilar.toml.example). `./ilar.toml` and
`./.ilar/ilar.toml` layer project settings over it, in that order. Nested
sections merge by field. Every file is read strictly: an unknown key or a
value of the wrong type stops startup with the file, the key and what was
expected. That includes a project file in a cloned repository: a typo there
keeps `ilar` from starting in that directory until the file is fixed or moved
aside (`--no-project-instructions` skips `AGENTS.md`, not `ilar.toml`). These settings are user-scoped and are not
overridden by project files:

- `general.theme` and `general.project_instructions`;
- the whole of `[providers]`, `[models]` and `[endpoints]` — where the
  conversation (prompts, code, tool output, your API key) is sent is never a
  cloned repository's call to make;
- `[cache_compact]`, which spends money on an unattended provider request;
- `[gateway]` and `[channels]`, which decide who may message your assistant;
- `[tool_review]`, which selects the optional tool-call reviewer;
- `[sandbox]`, which controls the experimental child-process sandbox.

A user-scoped table is discarded whole rather than merged field by field: a
project `[providers.openai]` cannot switch `auth` back, point `base_url`
elsewhere, or change anything else about a provider you configured. A project
file that sets one of these is reported rather than silently ignored — as a
system line in the TUI transcript at startup, on stderr from `ilar exec`, and
as a `{"type":"notice"}` event under `ilar exec --json`.

## Experimental tool-call review

Off by default. Add this table to your user config to record model reviews of
proposed tool calls:

```toml
[tool_review]
model = "custom/checker" # A model configured through [models] or a provider.
timeout_ms = 10000
adapter = "json" # Default. Use "shieldagent" for native ShieldAgent output.
```

The table enables observation only. No reviewer model is selected by default.
Project tables are ignored and reported. The timeout must be 1–60000 ms.
Each call adds a provider request and can incur a charge. Calls are reviewed
before tool dispatch, with up to four reviews at once, without a workspace lock.
A slow reviewer delays dispatch. The timeout is per review, not per tool batch:
eight stalled reviews can take about two timeout periods. There are no retries.
Total serialized input is limited to 128 KiB; oversized arguments produce an
error rather than a partial review. Collected answer text is limited to 8 KiB.
JSON review requests ask for at most 1024 output tokens; native ShieldAgent uses
128 and stops after its verdict. Some adapters, including ChatGPT authentication,
omit the wire token limit; timeout and cancellation still apply.

The reviewer receives exact proposed tool arguments, the working directory,
current sandbox settings/grants, up to eight recent recorded user messages,
and up to eight recent assistant/tool text records. Each history excerpt is
limited to 2048 bytes; images and reasoning are omitted. History is marked as
incomplete and untrusted evidence is separate from recorded user messages.
Recorded messages can include runtime notifications or delegated tasks; they
are not all new human authority. Project instructions and compaction summaries
are not promoted to user instructions. This is a bounded-context experiment,
not a proof that a call has user authorization.

Stored secret values are removed from history. If the secret store is locked
or unreadable, review is skipped and an error is recorded. Calls with stored secret values
in their arguments are not sent to the reviewer and receive an error record.
This is not general secret detection: other code, arguments, and tool output
can be sent to the selected model. Choose its endpoint accordingly.

The reviewer has no tools. The default `json` adapter expects `allow`, `ask`, or
`deny` with a reason. The `shieldagent` adapter uses its native prompt and maps
an initial `[Answer] safe` to allow and `[Answer] unsafe` to deny. It stops after
the verdict word and following whitespace, or accepts the word at normal end of
response. It records a fixed verdict description, not an analysis it did not
collect. ShieldAgent has no native `ask` result; malformed output is an error.
Timeouts, invalid output, and provider errors produce error records, not an
allow verdict. None of these results changes tool execution, existing approval
questions, or sandbox grants. Observation mode is not a safety control.
It covers root and child tool calls, direct file tools, and background launch
requests. It does not inspect commands later run inside scripts or services.
The `question` tool is excluded because it requests user input.

Results are `tool_review` events in session JSONL, linked by `tool_call_id`, with
model, adapter, verdict or error, reason, and elapsed milliseconds. They do not enter the
agent conversation or appear as approval prompts. Inspect a session file with:

```sh
jq 'select(.type == "tool_review")' /path/to/session.jsonl
```

### Local ShieldAgent on Apple Silicon

Use the original FP16 `thu-coai/ShieldAgent` weights. They occupy about 15.2 GB;
allow additional memory for model runtime and context. Do not select the 4-bit
conversion if you want the FP16 baseline: our small experiment found two new
unsafe approvals with that conversion.

The following setup uses the MLX versions tested here. Download once, then run
the server offline, bound to localhost only:

```sh
venv="$HOME/.local/share/ilar/shieldagent-venv"
model="$HOME/.local/share/ilar/models/ShieldAgent"
python3 -m venv "$venv"
"$venv/bin/pip" install 'mlx-lm==0.32.2' 'mlx==0.32.4'
"$venv/bin/hf" download thu-coai/ShieldAgent --local-dir "$model"
HF_HUB_OFFLINE=1 "$venv/bin/mlx_lm.server" \
  --model "$model" --host 127.0.0.1 --port 8766 \
  --temp 0 --max-tokens 128 --decode-concurrency 1 --prompt-concurrency 1 \
  --prompt-cache-size 2
```

Keep that terminal open. The server has no authentication; do not expose it to
other hosts or add a public tunnel. Stop it with Ctrl-C. Requests are served one
at a time with these settings, so queued reviews count against their timeouts.

Add these tables to your user `ilar.toml`, not the project's file:

```toml
[models.shieldagent]
base_url = "http://127.0.0.1:8766/v1"
model = "default_model" # MLX server alias for its --model argument.
context = 32768
output = 128

[tool_review]
model = "custom/shieldagent"
adapter = "shieldagent"
timeout_ms = 30000
```

Keep your normal agent model in `[general]` unchanged. Use a binary built after
the ShieldAgent adapter change (`scripts/install.sh` builds and installs it),
then start a new ilar runtime. Do not add a second `[tool_review]` table if one
already exists; change its fields instead.

From the repository root, check the local server and adapter without executing
any proposed command:

```sh
cargo test -p ilar --test smoke_tool_review -- --ignored --nocapture
```

The smoke test sends two synthetic proposals: a scoped Git-status read and a
private-key export. It checks allow/deny and prints the observation records.
Set `ILAR_SHIELDAGENT_URL` to test another endpoint. Actual runtime context is
not the old synthetic benchmark, so its measured accuracy and latency do not
carry over unchanged. This remains observation only, not automatic approval.

## Experimental process sandbox

Off by default. On macOS, `ilar --experimental-sandbox` and
`ilar exec --experimental-sandbox ...` enable read, write, and network restrictions
for shell-backed tools and `read`, `write`, `edit`, `glob`, and `grep`.
The same setting is available in the user config:

```toml
[sandbox]
enabled = true
network = false
profiles = ["cargo"]
read_paths = ["/absolute/path/to/a/reference-directory"]
write_paths = ["/absolute/path/to/a/build-cache"]
```

Project `[sandbox]` tables are ignored and reported. Paths must be absolute,
with no `..`; an explicit path grants its subtree. Unknown profile names
refuse process launch. Supported profiles are `cargo` (registry/git caches
and package-cache lock files), `python` (pip caches), and `node` (npm cache,
logs, and update-check marker). Profiles do not install binaries or enable
network. Symlinked cache locations need an explicit path grant. Parent cache
directories must already exist if creating them needs a wider grant.

Content reads are denied outside these defaults and explicit `read_paths`:

- The command workspace, including local dependencies and environments.
- macOS runtime paths (`/System`, `/bin`, `/sbin`, selected `/usr` subtrees),
  Apple developer tools, and specific system lookup/certificate files.
- Homebrew installation subtrees: `bin`, `sbin`, `lib`, `opt`, `Cellar`, and
  `share` under `/opt/homebrew`; equivalent selected `/usr/local` paths.
  General Homebrew `etc` contents are not included.
- `~/.cargo/bin`, `~/.rustup/toolchains`, `~/.rustup/settings.toml`, and
  `~/.local/share/mise/installs`. Other layouts need explicit grants.
- Read/write caches from selected profiles. No profiles are selected by default.

The whole home, `.config`, `.local`, and user `Library` trees are not granted.
User config files such as `.gitconfig`, `.cargo/config.toml`, and `.npmrc`
need explicit read grants; they can contain credentials. Symlinked user
installation roots need an explicit grant if they redirect elsewhere.
Filesystem metadata probes (existence, size, and similar attributes) remain
allowed globally. Listing the root directory is allowed for macOS startup,
but listing other ungranted directories or reading their files is not.

Writes are limited to the workspace, `/dev/null`, and configured paths/caches.
`read_paths` adds no writes; `write_paths` adds no reads. Set both for an external
read/write directory. Temporary-file environment variables point to the
workspace; there is no shared `/tmp` write grant. Network is denied unless
`network = true`, which permits all network access, not selected hosts.

For normal workspace operations, omit `sandbox`: workspace reads and writes are
already allowed. An inactive object (`{}` or empty path lists with false flags)
also uses the existing policy. Requests wholly covered by the workspace or
existing explicit grants do not prompt or need a reason. Requests for additional
access still need a reason and approval. Grant paths must be absolute; `.` is
not a grant path, and `/` requests access to the whole filesystem.

In the interactive root TUI, file tools, bash, service start, and sudo can request a
`sandbox` exception with `read_paths`, `write_paths`, `network`, or `unsandboxed`, plus a
`reason`. The user sees the command and access request before launch. Use
PageUp/PageDown to read long approval details. Choose "Allow this call only"
or "Allow for this session" for read paths, write paths, and network. Session
grants apply to later file-tool, bash, service start, and sudo calls without asking again,
including calls that omit the `sandbox` argument. Read grants do not add writes.
They are kept in memory for this live session only: closing/restarting its
runtime, or resuming it later, does not restore them. Other sessions and child
agents do not inherit them. Project-persistent approvals are not implemented.

Requested paths are resolved before approval, and the prompt shows the target
paths. Retained paths that later redirect through a symlink or cannot be resolved
are dropped; a new target needs approval. This does not remove the documented
filesystem race between checking a path and starting a command.

`unsandboxed` requests offer only once/deny, even when mixed with path/network
requests. Cancellation or missing approval support refuses new access. Child
sessions, `ilar exec`, and the gateway cannot approve these exceptions yet.
Configure their required grants before launch. A denied read returns an error,
not an automatic prompt. The agent must request an extension; ilar does not
infer a needed path or rerun partial work.

`unsandboxed` removes only ilar's sandbox for one approved call. Secret/root
checks and inherited external restrictions still apply. Failed commands are
not retried automatically by the sandbox. Experimental sudo also stops after
its first requested-command launch instead of retrying from diagnostic text.
Services keep their initial policy until restarted.

File tools run in the installed `ilar-file-worker` binary beside `ilar` or
`ilar-gateway`. Use `scripts/install.sh` to install all three. A missing helper
refuses the call; ilar does not search PATH or fall back to host file access.
File contents, image attachments, and edit digests cross a private pipe. The
parent retains approvals and workspace locks. Unlike a shell's direct write,
atomic file tools need read and write grants for the containing directory:
temporary siblings and the directory descriptor are part of replacement.
Requests that name an existing file as a write grant are refused before approval.
A configured exact-file grant or an unresolved file path does not widen access;
it can still fail during the operation.
Grep output files get a read-only grant for the tool-output directory, not the
whole state directory. External ignore files need explicit read grants; denied
walk entries can produce an empty search result rather than an error.

This is not a complete security boundary. Git helpers, provider requests,
and other host-side operations are not confined by this process policy.
Metadata disclosure, IPC, inherited descriptors, and broker
services remain outside its protection. Secrets inside an allowed workspace
or toolchain path are readable. Do not use it to run hostile code.

The macOS backend uses the deprecated system `sandbox-exec` helper. An enabled
sandbox fails on unsupported platforms or activation errors; it never falls
back to an unrestricted shell. Some outer sandboxes prevent activation.
Observe/training mode, saved project grants, and a Linux backend remain future
work. See [experiments and limits](../experiments/sandbox/README.md).

## Settings

| Setting | Default | Description |
| --- | --- | --- |
| `general.model` | `zai/glm-4.7` | Default `provider/model-id`. |
| `general.reasoning` | provider default | Default reasoning variant for new sessions (for example `low`, `high`, or `max`; model-specific). Set `default` in a higher config layer to clear an inherited value. It must be valid for `general.model`; a launch that runs some other model (`--model`, or an agent's own `model:`) drops it with a startup line rather than refusing to start. |
| `general.replay_thinking` | `all` | How much of a chat-wire model's thinking goes back to it: `all`, `turn` or `off`. See [thinking on the wire](#thinking-on-the-wire); a `[models.*]` or `[endpoints.*]` entry can override it for its own server. |
| `general.theme` | `carbon` | See [themes](interface.md#themes). F3 opens the picker. |
| `general.project_instructions` | `true` | Whether the working directory's `AGENTS.md`/`CLAUDE.md` is part of the system prompt. See [Project instructions](#project-instructions). User-scoped. |
| `general.memory` | `true` | Whether a terminal session remembers across sessions, one store per project (a repository and all its worktrees) under `<state dir>/memory/`. `false` leaves the model without the `memory` tools and the prompt without the core block and the "Remembering" section; nothing on disk is touched. See [memory](sessions.md#memory-that-outlives-a-session). |
| `general.memory_recall` | `false` | Whether each prompt surfaces the notes a selector model picks for it (up to five, from their title and summary), after the message, with their bodies (see `memory_recall_bodies`). See [memory](sessions.md#memory-that-outlives-a-session). |
| `general.memory_recall_model` | `"openai/gpt-6-luna"` | The selector's model, or `"main"` for the conversation's own. It reads one line per note and the message, at its lowest effort; a failed or slow call costs the prompt its recall, not the turn. |
| `general.memory_recall_history` | `true` | The selector reads the session's earlier messages and the picks it gave them, as Claude Code's does, so a follow-up ("are you sure?") is read in context. `false`: each message alone. |
| `general.memory_recall_bodies` | `true` | A recall carries each note's body (up to 4 KB), as Claude Code's does. `false`: the index line alone, read in full with `memory_get`. |
| `general.memory_index` | `true` | Whether a session opens with every note's index line (up to 200 lines / 25 KB, newest first) beside the core block. |
| `general.resume_offer` | `true` | Whether a bare `ilar` offers this directory's last session — its tail shown ghosted, Enter resumes it. See [Starting](interface.md#starting). |
| `providers.openai.base_url` | API or ChatGPT endpoint | Override the Responses API base URL selected by `auth`. |
| `providers.openai.api_key` | `ILAR_OPENAI_API_KEY` | OpenAI API key. |
| `providers.openai.auth` | `api_key` | `api_key` or `chatgpt`; see [OpenAI ChatGPT OAuth](#openai-chatgpt-oauth). Either credential also enables the `image_gen` tool. |
| `providers.openai.image_gen` | `true` | `false` leaves the `image_gen` tool out even with a credential. |
| `providers.zai.base_url` | `https://api.z.ai/api/coding/paas/v4` | Override the z.ai OpenAI-compatible base URL. |
| `providers.zai.api_key` | `ILAR_ZAI_API_KEY` | z.ai API key. |
| `providers.opencode.base_url` | `https://opencode.ai/zen/v1` | Override the OpenCode Zen base URL. |
| `providers.opencode.api_key` | `ILAR_OPENCODE_API_KEY` | OpenCode API key, used as `opencode/<model>`. See [OpenCode Zen and Go](#opencode-zen-and-go). |
| `providers.opencode-go.base_url` | `https://opencode.ai/zen/go/v1` | Override the OpenCode Go base URL. |
| `providers.opencode-go.api_key` | `ILAR_OPENCODE_API_KEY` | The same key, used as `opencode-go/<model>`. |
| `models.<name>.*` | — | Your own OpenAI-compatible endpoint, used as `custom/<name>`. See [Bring your own model](#bring-your-own-model). |
| `endpoints.<name>.*` | — | An OpenAI-compatible server whose models are discovered from its listing, each used as `<name>/<id>`. See [Discover an endpoint's models](#discover-an-endpoints-models). |
| `agent.max_iterations` | `1000` | Max provider calls per user turn (runaway-loop backstop). |
| `agent.sudo` | `false` | Install the `sudo` tool: one command as root after the person has read it and said yes. See [Secrets](secrets.md#sudo). |
| `agent.max_output_tokens` | `32768` | Most tokens one response may produce, sent as the wire's cap (`max_tokens` or `max_output_tokens`); `0` sends none. A response cut at the cap says so in its text. One tool call's arguments are cut at 1 MiB regardless. |
| `compaction.threshold` | `0.85` | Context fraction at which history is handed over; must be between 0 and 1. |
| `cache_compact.enabled` | `false` | Compact an idle session once, just before its last request leaves the provider's prompt cache. See [Compacting while the cache is warm](#compacting-while-the-cache-is-warm). User-scoped. |
| `cache_compact.margin_secs` | `60` | How long before the cache window closes to fire. |
| `cache_compact.context_floor` | `150000` | Below this many context tokens a cold read is cheap and nothing fires. |
| `cache_compact.ttl_secs.<provider>` | `openai` 1800, others 300 | The cache window assumed per provider prefix, in seconds. |
| `subagents.max_concurrent` | `10` | Maximum concurrent subagents; must be at least 1. |
| `subagents.max_depth` | `3` | Maximum nested subagent depth; must be at least 1. |
| `subagents.background_tool_timeout_ms` | `600000` | Background tool timeout in milliseconds; must be at least 1. |
| `gateway.*` | — | The assistant gateway's settings (`ilar-gateway`); parsed by that crate. User-scoped. |
| `channels.<name>.*` | — | One messaging channel the gateway listens on. User-scoped. |

Environment variables:

| Variable | Purpose |
| --- | --- |
| `ILAR_CONFIG_DIR` | Replaces the default `~/.config/ilar` user configuration directory. |
| `ILAR_STATE_DIR` | Replaces the default `~/.local/state/ilar` session, authentication and spilled-tool-output directory. |
| `ILAR_OPENAI_API_KEY` | Fallback OpenAI API key. |
| `ILAR_ZAI_API_KEY` | Fallback z.ai API key. |
| `ILAR_OPENCODE_API_KEY` | Fallback OpenCode key, for both Zen and Go. |
| `ILAR_TAVILY_API_KEY` | Switches web search to the Tavily API (recommended). |
| `ILAR_EXA_API_KEY` | Authenticates the default Exa web search backend. |
| `ILAR_SERVE_TOKEN`, `ILAR_SERVE_POLL_MS` | For `ilar serve` only; see [serve](serve.md). |

A provider key may also live in the [secret store](secrets.md) under
its variable's name (`ilar secret set ILAR_OPENAI_API_KEY`); the TOML
field wins, then the environment, then the store. None of these
reach a shell the model runs. A variable set to the empty string reads
as unset throughout.

Both directory defaults hang off `HOME`. With `HOME` unset, `ilar` and
`ilar-gateway` refuse to start unless `ILAR_CONFIG_DIR` and `ILAR_STATE_DIR`
are both set: the defaults would otherwise resolve to `./.config/ilar` and
`./.local/state/ilar` and scatter a separate set of sessions and secrets
through every directory they were launched from.

`ilar secret …` and `ilar login` resolve these two directories and nothing
else — no `ilar.toml` is parsed and no endpoint is contacted — so a config
file that fails to parse never stands between you and the credential you
came to fix.

### Allowed subagent models

Open the model picker with `F2` or `Ctrl+X`, then `M`. Each row has
`[S]` if that model is allowed for subagents. `Ctrl+A` toggles the selected
row without changing the main agent's model. `Ctrl+R` restores the default:
only the immediate parent's current model is allowed, even after a model switch.
`Enter` still selects the main model and its reasoning level.

The first toggle changes the default into an explicit list. Only listed
models are then allowed; the parent's model is not automatically added.
Changes are saved in the current session and restored when you resume it.
Other sessions keep their own permissions. New sessions use the same-as-parent
default; forks inherit the permissions at the fork point. Child agents use
their root session's permissions. These settings are not written to `ilar.toml`.

Clear every `[S]` mark to block all subagent starts. Use `Ctrl+R` to restore
the same-as-parent default. Agent definitions and explicit task model
overrides cannot bypass the restriction. A resumed subagent keeps its saved
model, so its resume is refused if that model is no longer allowed.
Already-running subagents can finish.

### The stall watchdog

`subagents.background_tool_timeout_ms` bounds one *tool call* a
background job makes. A separate, non-configurable watchdog bounds a
detached **task**: if no event reaches it — from the task's own turn or
from any foreground descendant — for **600 seconds**, the task is
stopped and reports `Task "X" stalled: no progress for 600s. It has
been stopped.` The agents panel shows the silence building as `· quiet
45s` on the task's row well before that, so the watchdog's verdict is
never the first news of a hang. A task queued behind another for the
same checkout says `· waiting for the workspace` instead: it is not
silent, it is in line, and the watchdog's clock does not start until it
has the lease.

## Web search

The `websearch` tool works out of the box: without any configuration it calls
the hosted [Exa](https://exa.ai) MCP endpoint anonymously. Keyless access is
best-effort and rate-limited by Exa, so for real use you should bring your own
key — either `ILAR_TAVILY_API_KEY` to use Tavily, or `ILAR_EXA_API_KEY` to
authenticate against Exa. If both are set, Tavily wins. A throttled or
refused search says so and names both variables, so the model can pass
the fix on instead of retrying.

## Bring your own model

Any server that speaks the OpenAI `/chat/completions` dialect can be a model in
ilar: llama.cpp, ollama, vLLM, LM Studio, a company gateway, a third-party API.
Each one is a `[models.<name>]` section, and each one becomes the model id
`custom/<name>` — selectable in the picker, usable as an agent's `model`, listed
by the `models` tool.

| Field | Required | Default | Description |
| --- | --- | --- | --- |
| `base_url` | yes | — | Everything up to `/chat/completions`, e.g. `http://127.0.0.1:8080/v1`. Must be an `http://` or `https://` URL with a host and no query or fragment; a trailing slash is dropped. The same rule applies to every provider's `base_url`. |
| `context` | yes | — | The window the endpoint serves, in tokens. There is no catalog row behind a custom model, so this number is the only thing input budgeting and compaction have; see below. |
| `model` | no | the section name | The id to put on the wire, when the server calls the model something else (`llama3.3:70b`). |
| `api_key` | no | none | Sent as `Authorization: Bearer …`. With no key, **no Authorization header is sent at all** — which is what a local server wants. There is no environment-variable fallback for these entries. |
| `output` | no | a quarter of `context` | Tokens reserved for the reply. A local server's window is one budget shared by prompt and reply, so some of it is held back. |
| `vision` | no | `false` | Whether the model accepts images. When false, images in the session are replaced with `[image omitted: this model cannot view images]` rather than refused. |
| `replay_thinking` | no | `[general]`'s, else `all` | How much of the model's thinking goes back to it: `all`, `turn` or `off` (see [thinking on the wire](#thinking-on-the-wire)). `off` is for a server that streams reasoning but refuses the field as input. |
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
`openai`, `zai`, `opencode`, `opencode-go` or `custom` — a section named after a
provider would produce an id nothing could resolve.

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

`[models]` is user configuration only: a project file declaring model entries
is warned about at startup and ignored wholesale, because a repository must
not be able to route the conversation to an endpoint it chose. The same rule
covers `[providers]` — a project `base_url` would send requests carrying
*your* key wherever it pointed.

## Discover an endpoint's models

A server that lists what it serves — Lemonade, llama.cpp, vLLM, LM
Studio, anything answering `GET /models` in the OpenAI shape — can be
one `[endpoints.<name>]` section instead of a `[models.*]` section per
model. Its models are discovered when configuration loads and appear
as `<name>/<id>`: in the model picker (F2), in the gateway's `/model`, as an agent's `model`.

```toml
[endpoints.lemonade]
base_url = "http://127.0.0.1:13305/api/v1"
context = 65536          # for models whose listing does not say
```

| Field | Required | Default | Description |
| --- | --- | --- | --- |
| `base_url` | yes | — | Everything up to `/models` and `/chat/completions`; the same rule as a model's. |
| `api_key` | no | none | Sent as `Authorization: Bearer …` on the listing and every request. |
| `context` | no | `32768` | The window for a model whose listing does not state one. |
| `output` | no | a quarter of the window | Tokens reserved for the reply. |
| `vision` | no | `false` | For models whose listing does not say. |
| `replay_thinking` | no | `[general]`'s, else `all` | As for `[models.*]`: `all`, `turn` or `off`. |
| `models` | no | all | Only these ids. |
| `options` | no | none | Body fields merged into every request. |

What the listing says wins: Lemonade's carries `labels` — only models
labelled `chat` are taken, `vision` sets vision — and `context_length`,
so those models get their own window. A plain listing without labels,
as llama.cpp and vLLM produce, contributes every model on the
endpoint's defaults. A model marked not downloaded is left out.

The listing is fetched with a three-second timeout and kept under
`<state dir>/endpoints/<name>.json`. A server that is down at start
still yields the models it listed last time, with a warning; one never
reached yields none, also with a warning. Like `[models]`, the table
is user configuration: a project file that declares it is ignored.

## OpenCode Zen and Go

[OpenCode Zen](https://opencode.ai/docs/zen/) is a pay-as-you-go gateway to
many vendors' models; [OpenCode Go](https://opencode.ai/docs/go/) is a monthly
subscription to a smaller lineup with usage caps. One API key from the OpenCode
console serves both. Set `ILAR_OPENCODE_API_KEY` (or `api_key` in either
section) and the models appear in the picker as `opencode/<id>` and
`opencode-go/<id>`, spelled exactly as opencode itself spells them:

```toml
[general]
model = "opencode-go/glm-5.3"

[providers.opencode-go]
# api_key from ILAR_OPENCODE_API_KEY by default; can be set here.
```

The catalog records each model's wire and ilar routes accordingly, so a
model id is all you choose. GPT, Grok, Muse, and Crouton use the OpenAI
Responses API; the other catalog rows use chat-completions. The Qwen and
MiniMax rows the docs file under the Anthropic Messages wire answer on
chat-completions too and are offered there; the Claude and Gemini families
are not, since ilar speaks neither of their wires.

Reasoning levels are per model, as opencode publishes them (models.dev
`reasoning_options`): the GPT rows keep their `openai/` windows and ladders,
Muse Spark and Grok have their own, and on the chat wire GLM, DeepSeek, Kimi,
Qwen, Hy, and Mistral each carry the rungs they accept. Mistral Large 4 uses
`low`, `medium`, and `high`: live requests and the AI SDK documentation override
the stale `none/high` metadata. `general.reasoning = "high"` reaches the
Responses wire as `reasoning.effort` and the chat wire as `reasoning_effort`;
a level the model does not list is refused.

Prices are the current published base text rates, including cache reads and
writes. Cost estimates do not apply long-context tiers or scheduled discounts.
For example, Grok 4.7 uses $2 input / $6 output per million tokens in the catalog;
the provider charges $4 / $12 above 200K context. Go uses published list prices
to count subscription usage, so local estimates can differ from its meter.

A listed free model can still be blocked by account limits or regional rules.
Zen Crouton Alpha Free and Nemotron 3.5 Super VL Free returned an account-level
free-usage limit during the catalog check; Go Crouton completed a tool call.
Zen Qwen3.8 Max returned an access-disabled error for the test key.

### Thinking on the wire

On the chat wire a model's thinking goes back to it. The GLM, DeepSeek,
Kimi, MiniMax and Qwen families think *interleaved* with their tool calls,
and their templates read `reasoning_content` off the assistant messages so
the model holds its own plan through a tool loop. Without it every step
started from an empty think block and the model made up what it had been
thinking — a Qwen3.8 session produced eleven thoughts about a previous
reply it never gave.

How much goes back is `replay_thinking`, in `[general]` for every model and
overridable per `[models.<name>]` or `[endpoints.<name>]` entry:

- `all` (the default, and what OpenCode sends): every assistant message in
  the conversation carries its thinking. Costly in context, but if the model
  was trained with its whole history of thought in front of it, the
  alternative starves it.
- `turn`: only the assistant messages after your last message — the
  vendors' documented minimum, where the Qwen and GLM templates keep it and
  DeepSeek's own docs say to drop the rest.
- `off`: none, for a server that streams reasoning but refuses the field as
  input; an OpenAI-strict validator rejects unknown assistant fields.

Some models never use the field on the way *out*: MiniMax M3, and older
Qwen builds, open their answer with a literal `<think>…</think>` block in
the content itself. A block at the head of the content stream is read as
thinking and nothing else has to be configured — the tag may arrive split
across deltas and still closes. Only a leading block counts, so a model
writing *about* the tag later in an answer is left alone. The line break
between the thought and the answer is a separator and comes off; an
answer that opens with indented code keeps its indent. What was
extracted is thinking like any other, so it goes back under
`replay_thinking` as `reasoning_content` — verified live against
`opencode-go/minimax-m3` over two turns on 2026-09-19. A block the model
never closes stays thinking to the end of the turn, the same as a
reasoning model truncated mid-thought.

A thought goes back under the name it arrived as: `reasoning_content` for
most, `reasoning` for the OpenRouter-style servers behind OpenCode Zen. The
same holds for every model a discovered `[endpoints.<name>]` or
`[models.<name>]` server serves: if it streamed thinking, it is offered back
— llama.cpp, vLLM, LM Studio and Lemonade use it, and a shim that ignores
the field loses nothing. The Responses wire has its own reasoning items, and
OpenAI's chat wire hands no thinking back at all, so their thinking stays
where it was, as a local note in the log.

## Compacting while the cache is warm

A session that idles past the provider's prompt-cache window pays a cold
re-read of its whole context on the next move, whatever that move is. For
the minutes before the window closes, compaction is cheap: the summary
request reads the big context at cached rates and leaves a small one
behind. With `cache_compact.enabled = true`, an idle session whose context
is above `context_floor` compacts once, `margin_secs` before the assumed
window (`ttl_secs` per provider) closes, counted from its last completed
request. It never fires while a turn runs, while a message is queued or a
result is being delivered, or under a modal, and only once per idle
episode; the next turn starts a new one. The transcript gets a line when it
fires and a standing notice when it lands. Nothing is lost: what left the
model's context is still in the session log, and the agent's own `history`
tool searches it, summarized turns included.

It is off by default because it sends a provider request with nobody
watching, and it is user configuration: a project file cannot turn it on.
The TUI runs it; the parked `serve` driver does not yet.

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
   `${ILAR_CONFIG_DIR:-~/.config/ilar}/ilar.toml`. `ilar login` prints these
   lines when it succeeds, since stored tokens nothing points at change
   nothing:

   ```toml
   [general]
   model = "openai/gpt-6.1-sol"

   [providers.openai]
   auth = "chatgpt"
   ```

   ChatGPT uses its Codex model catalog rather than the standard API-key model
   catalog. `openai/gpt-6.1-sol` is one supported example; the in-app model
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
