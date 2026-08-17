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
  Esc = full abort.

## Status

Pre-alpha. See `meta/issues.md` for the roadmap and
[DEVLOG.md](DEVLOG.md) for design notes and research findings.

## Configuration

`~/.config/ilar/ilar.toml` (see `ilar.toml.example`), custom agents as
Markdown in `~/.config/ilar/agents/` or project `.ilar/agents/`, and skills in
`~/.config/ilar/skills/` or project `.ilar/skills/`.
