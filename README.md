# ilar

A personal coding agent in Rust. Single binary, TUI-first, no permissions layer
(runs inside a kernel-restricted sandbox).

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
- **No permission system.** The sandbox is the permission system.
- **Skills over features:** anything exotic (e.g. git worktree isolation)
  is a markdown skill, not core code.

## Crates

- `ilar` — core: providers, tools, agent loop, sessions, config. Pure logic,
  no TUI dependencies. Unit-testable with mock SSE streams.
- `ilar-tui` — ratatui frontend: streaming output, tool display, input box,
  Enter sends, Shift-Enter or Ctrl-J inserts a newline, Esc = full abort.

## Status

Pre-alpha. See `meta/issues.md` for the roadmap and
[DEVLOG.md](DEVLOG.md) for design notes and research findings.

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
| `providers.openai.auth` | `api_key` | `api_key` or `chatgpt`; run `ilar login` before using `chatgpt`. |
| `providers.zai.base_url` | z.ai endpoint for `flavor` | Override the z.ai API base URL selected by `flavor`. |
| `providers.zai.api_key` | `ILAR_ZAI_API_KEY` | z.ai API key. |
| `providers.zai.flavor` | `anthropic` | `anthropic` or `openai`. |
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
| `ILAR_TAVILY_API_KEY` | Enables Tavily-backed web search. |

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
`model`, `disabled`, and `read_only`. A file with `disabled = true` is skipped;
it does not remove a lower-priority definition with the same name.

Skills are Markdown files in `${ILAR_CONFIG_DIR:-~/.config/ilar}/skills/` and
`./.ilar/skills/`. Project skills override user and built-in skills with the
same filename. Root sessions list their names and descriptions in the system
prompt and load full bodies on demand through the `skill` tool. Skill
frontmatter supports `description`; `triggers` is accepted as reserved metadata
but is not currently interpreted.

Built-in subagents are `build` (mutable, serialized per checkout) and `explore`
(read-only, safe for parallel repository inspection and review).
