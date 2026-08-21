# ilar

A personal coding agent in Rust. Single binary, TUI-first.

## Safety

> [!WARNING]
> ilar does **not** provide a sandbox, permission prompts, or an access-control
> boundary. It can run shell commands and read, modify, or delete anything that
> its process can access, including credentials and files outside the current
> repository.

Run ilar only inside an external, OS-enforced sandbox with appropriately scoped
filesystem, network, process, and credential access. Options include
[Agent Safehouse](https://agent-safehouse.dev/), [nono](https://nono.sh/), a
locked-down container, or a dedicated virtual machine.

Git worktrees and ilar's read-only/mutating tool scheduling are coordination
mechanisms, not security boundaries.

## Design principles

- **One event loop, no hidden runtimes.** The agent loop is a plain async
  state machine over an event channel. Subagents are `tokio` tasks, not
  processes.
- **Providers we actually use:** OpenAI (Responses API) and z.ai (GLM,
  Anthropic-compatible and OpenAI-compatible endpoints).
- **Concurrency barrier:** tools declare themselves read-only or mutating.
  Mutating tools form a barrier; read-only tools run concurrently
  (the Claude Code `isConcurrencySafe` model).
- **JSONL sessions:** append-only, human-readable, resumable.
- **No built-in permission system.** An external sandbox is the security
  boundary; ilar itself does not create or enforce one.
- **Skills over features:** anything exotic (e.g. git worktree isolation)
  is a markdown skill, not core code.

## Crates

- `ilar` — core: providers, tools, agent loop, sessions, config. Pure logic,
  no TUI dependencies. Unit-testable with mock SSE streams.
- `ilar-tui` — ratatui frontend: streaming output, tool display, input box,
  Enter sends, Shift-Enter or Ctrl-J inserts a newline, Esc or Ctrl-C =
  full abort, Ctrl-D on a blank prompt quits.

## Status

Pre-alpha. See `meta/issues.md` for the roadmap and
[DEVLOG.md](DEVLOG.md) for design notes and research findings.

## Status line

During a turn the status line reads like:

```
○ thinking · 84.2 KiB · 12.3 KiB/s   zai/glm-5.3   in 300 · out ~8.4k · req cache r1838/w0 · Σ 1.2M $0.42 · ctx [██░░░░░░] 24%
```

- **Activity + liveness** — `thinking · 84.2 KiB · 12.3 KiB/s`: bytes
  streamed this turn and the current transfer rate. A silent stream shows
  `· no data Ns` after 3 seconds; `0 B · no data Ns` means the provider
  has not sent a single byte. The spinner alone proves nothing — only
  these numbers do.
- **`in` / `out`** — the last provider request's token usage. While a
  step streams, `out ~N` is a live estimate from streamed bytes
  (~4 bytes/token) and snaps to the exact reported value when the step
  completes.
- **`req cache rN/wM`** — prompt-cache accounting for the last request:
  `r` tokens were read from the provider's prompt cache (billed at the
  cheap cache-read rate), `w` tokens were written as new cache entries
  (an Anthropic-style charge; OpenAI-compatible endpoints report 0).
  A healthy agentic session shows a large, growing `r`; a sudden drop to
  0 means the cached prefix was invalidated (model switch, prompt change,
  or provider eviction) and per-step cost/latency just went up.
- **`Σ tokens $cost`** — session-cumulative totals across all turns,
  priced per-step at each model's list rates (cache reads at the cache
  rate). Coding-plan models show `plan` instead of dollars; unknown
  models show tokens only. The palette's "Session usage" entry has the
  full breakdown.
- **`ctx …%`** — estimated context usage against the model's window
  (`~` marks estimates); compaction triggers at `compaction.threshold`.

## Configuration

The user configuration is `${ILAR_CONFIG_DIR:-~/.config/ilar}/ilar.toml`; see
[`ilar.toml.example`](ilar.toml.example). `./ilar.toml` and
`./.ilar/ilar.toml` layer project settings over it, in that order. Nested
sections merge by field. `general.theme` is user-scoped and is not overridden
by project files.

| Setting | Default | Description |
| --- | --- | --- |
| `general.model` | `zai/glm-4.7` | Default `provider/model-id`. |
| `general.theme` | `terminal` | `terminal`, `carbon`, `parchment`, `frost`, or `high-contrast`. |
| `providers.openai.base_url` | API or ChatGPT endpoint | Override the Responses API base URL selected by `auth`. |
| `providers.openai.api_key` | `ILAR_OPENAI_API_KEY` | OpenAI API key. |
| `providers.openai.auth` | `api_key` | `api_key` or `chatgpt`; see [OpenAI ChatGPT OAuth](#openai-chatgpt-oauth). |
| `providers.zai.base_url` | z.ai endpoint for `flavor` | Override the z.ai API base URL selected by `flavor`. |
| `providers.zai.api_key` | `ILAR_ZAI_API_KEY` | z.ai API key. |
| `providers.zai.flavor` | `anthropic` | `anthropic` or `openai`. |
| `agent.max_iterations` | `1000` | Max provider calls per user turn (runaway-loop backstop). |
| `compaction.threshold` | `0.85` | Context fraction at which history is summarized; must be between 0 and 1. |
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

#### Web search

The `websearch` tool works out of the box: without any configuration it calls
the hosted [Exa](https://exa.ai) MCP endpoint anonymously. Keyless access is
best-effort and rate-limited by Exa, so for real use you should bring your own
key — either `ILAR_TAVILY_API_KEY` to use Tavily, or `ILAR_EXA_API_KEY` to
authenticate against Exa. If both are set, Tavily wins.

### OpenAI ChatGPT OAuth

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

### Project instructions

ilar reads `AGENTS.md`, or `CLAUDE.md` when `AGENTS.md` is absent, from exactly
two locations:

- `${ILAR_CONFIG_DIR:-~/.config/ilar}/`
- The current working directory

When both locations contain instructions, user instructions are included first
and working-directory instructions second. ilar does not search parent
directories or combine instructions from an ancestor tree.

### Agents and skills

Custom agents are Markdown files in
`${ILAR_CONFIG_DIR:-~/.config/ilar}/agents/` and `./.ilar/agents/`. Project
definitions override user definitions with the same filename, and user
definitions override built-ins. Agent frontmatter supports `description`,
`model`, `disabled`, `read_only`, and `tools` (an allowlist of tool names the
agent may use; unknown names are a load-time error, and the list intersects
with the read-only set when `read_only = true`). Tool restriction is
coordination, not a security boundary. A file with `disabled = true` is
skipped; it does not remove a lower-priority definition with the same name.

Skills are Markdown files in `${ILAR_CONFIG_DIR:-~/.config/ilar}/skills/` and
`./.ilar/skills/`. Project skills override user and built-in skills with the
same filename. Root sessions list their names and descriptions in the system
prompt and load full bodies on demand through the `skill` tool. Skill
frontmatter supports `description` and `triggers` (a list of cue phrases
included in the system-prompt listing so the model invokes the skill when they
match the task). In the TUI, typing `/` shows inline completion for
skills and built-in commands (Tab or Enter accepts); `/<skill-name>
[arguments]` invokes a skill directly, and the palette's "Invoke skill…"
entry opens a picker.

Built-in subagents are `build` (mutable, serialized per checkout) and `explore`
(read-only, safe for parallel repository inspection and review). Tasks can
override the child's model per invocation (`model` and `reasoning` on the task
tool — e.g. a cheap flash model for mechanical sweeps); omitted, the child uses
the agent definition's model or inherits the parent's model and reasoning. The
read-only `models` tool lists available models with context windows, pricing,
and reasoning variants so agents can choose informedly.

### Services

The `service` tool manages long-running processes (dev servers,
watchers): `start {name, command}`, `status`, `logs`, `stop`. Services
keep running between tool calls, their combined output is retained in a
bounded buffer, and **everything is killed when the session ends or
switches** — no orphaned servers. Running services appear in the sidebar
and in the pending manager (Ctrl-Q), where a confirmed `d d` stops them
all. Subagents share the session's services. Note that foreground bash
deliberately kills its process group on completion, so this tool is the
supported way to keep a server alive.

### Goal mode

`/goal <description>` keeps ilar working until the goal is demonstrably
achieved: after every completed turn it auto-continues (in the same
session, so the prompt cache absorbs the cost) with an instruction to
verify progress using concrete evidence — running tests or a harness,
building one if none exists — and to keep working otherwise. The loop
ends when the model outputs an evidenced `GOAL_ACHIEVED:` line, when the
round cap (25) trips, or when you abort it explicitly (`/goal abort` or
the pending manager). `/goal` alone prefills the input for editing the
goal in place, keeping the round budget. Aborting a running turn pauses
the loop; it resumes after your next completed turn.

Type while a turn is running and the message **steers** it: the loop
delivers it at the next step boundary rather than after the whole task,
and a steer arriving as the model stops reopens the turn instead of
stranding the message. The transcript line appears when the model
actually receives it, and the input title shows how many are in flight.
If a turn ends without delivering one — you aborted, or it errored — the
undelivered steers move to the queue rather than vanishing. Turns with
no steer channel (a notification routed from another session) still
queue as before.

Standing state — queued messages, the goal, background jobs, a retry
offer — is managed in the pending manager (**Ctrl-Q** or the palette):
delete one queued message, pull it back into the input for editing,
abort the goal or cancel background jobs (both confirmed with a second
press). **Esc is strictly immediate-scope**: it aborts the running turn
or clears the input, and never touches the queue or the goal.

### Commands

Commands are markdown whose body *is* the prompt. Unlike skills they are
never listed in the system prompt and the model can never invoke one:
`/name args` substitutes `$ARGUMENTS` (or `$1`, `$2`, …) into the body
and submits it directly. Put them in `~/.config/ilar/commands/` or a
project's `.ilar/commands/`.

```markdown
---
description: Address Greptile PR comments
---
Address Greptile feedback on the current pull request.

Command arguments: $ARGUMENTS
```

Frontmatter is TOML or YAML, so opencode and Claude Code command files
work unchanged. `$` is otherwise left alone — `$(date)`, `${HOME}` and
`$NAME` pass through — but `$` followed by a digit is always a
placeholder, so an unmatched one expands to nothing rather than staying
literal. A command sharing a name with a skill shadows it; `goal` is
reserved for the built-in.

### MCP

ilar deliberately ships no built-in MCP client. The built-in `mcp-via-cli`
skill teaches the agent to drive MCP servers through the external
[`mcptools`](https://github.com/f/mcptools) CLI instead (stdio and HTTP
servers, config discovery from common `mcp.json` locations). MCP servers run
outside ilar with whatever access your sandbox grants; ilar adds no credential
handling.
