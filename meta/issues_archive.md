# Completed issues

## Milestone 1 — Lean core

- [x] [Session model + JSONL persistence + resume](issues/session-jsonl.md)
- [x] [Provider trait + event model](issues/provider-trait.md)
- [x] [OpenAI Responses API provider (streaming)](issues/provider-openai-responses.md)*
- [x] [z.ai GLM provider (Anthropic-compatible + OpenAI-compatible)](issues/provider-zai.md)
- [x] [Core tools: read, write, edit, bash, glob, grep](issues/core-tools.md)
- [x] [Concurrency-barrier tool executor](issues/tool-executor-barrier.md)
- [x] [Agent loop (turn state machine over event channel)](issues/agent-loop.md)
- [x] [Config: TOML + markdown agent definitions + AGENTS.md injection](issues/config-and-agents-md.md)
- [x] [Minimal TUI: streaming, tool display, input, Esc full-abort](issues/minimal-tui.md)

## Milestone 2 — Multiply

- [x] [Task tool: parallel subagents with child sessions](issues/task-tool-subagents.md)
- [x] [Background agents + completion notifications](issues/background-agents.md)
- [x] [Auto-compaction](issues/auto-compaction.md)

## Milestone 3 — Polish & extras

- [x] [Todo tool](issues/todo-tool.md)
- [x] [Web fetch + web search tools](issues/web-tools.md)
- [x] [Runtime model switching](issues/model-switching.md)
- [x] [Skills (markdown, incl. git-worktree-isolation skill)](issues/skills.md)

\* live smoke test still pending: no OpenAI API key available (both local
installs use ChatGPT OAuth). Fixture tests pass; run
`cargo test -p ilar --test smoke_zai` style live checks once a key exists.

## Follow-ups

- [x] [OpenAI ChatGPT OAuth login (PKCE)](issues/openai-oauth-login.md)
- [x] [Tool call stalls after todo](issues/tool-call-stalls-after-todo.md)
- [x] [Markdown transcript rendering](issues/markdown-transcript-rendering.md)
- [x] [Transcript scrolling](issues/transcript-scrolling.md)
