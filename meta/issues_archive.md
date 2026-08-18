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
- [x] [Tool registry uniqueness](issues/tool-registry-and-scheduling-invariants.md)
- [x] [Fix z.ai OpenAI wire format](issues/fix-zai-openai-wire-format.md)
- [x] [Robust bash execution](issues/robust-bash-execution.md)
- [x] [Crash-safe session recovery](issues/crash-safe-session-recovery.md)
- [x] [Session writer lease](issues/session-writer-lease.md)
- [x] [Provider routing and model lifecycle](issues/provider-routing-and-model-lifecycle.md)
- [x] [Preserve provider content order and reasoning](issues/preserve-provider-content-order-and-reasoning.md)
- [x] [Serialize turns and route notifications](issues/serialize-turns-and-route-notifications.md)
- [x] [Atomic file replacement](issues/atomic-file-replacement.md)
- [x] [Secure atomic OAuth store](issues/secure-atomic-oauth-store.md)
- [x] [Atomic file tools and large reads](issues/atomic-file-tools-and-large-reads.md)
- [x] [Background tool calls](issues/background-tool-calls.md)
- [x] [TUI tool details and telemetry](issues/tui-tool-details-and-telemetry.md)
- [x] [Model catalog and picker](issues/model-catalog-and-picker.md)
- [x] [Default and maximum context windows](issues/default-and-maximum-context-windows.md)
- [x] [TUI cursor and markdown density](issues/tui-cursor-and-markdown-density.md)
- [x] [Collapse Markdown separator rows](issues/collapse-markdown-separator-rows.md)
- [x] [Tool scheduling and workspace capabilities](issues/tool-scheduling-and-workspace-capabilities.md)
- [x] [Compaction correctness and cancellation](issues/compaction-correctness-and-cancellation.md)
- [x] [Harden provider protocol handling](issues/harden-provider-protocol-handling.md)
- [x] [Bound and sanitize web tools](issues/bound-and-sanitize-web-tools.md)
- [x] [Subagent safety and outcomes](issues/subagent-safety-and-outcomes.md)
- [x] [Fix subagent launch regression](issues/fix-subagent-launch-regression.md)
- [x] [Select and copy transcript text](issues/select-and-copy-transcript-text.md)
- [x] [Persist and render todos](issues/persist-and-render-todos.md)
- [x] [Move todos to a sidebar](issues/move-todos-to-sidebar.md)
- [x] [Config loading and frontmatter diagnostics](issues/config-loading-and-frontmatter-diagnostics.md)
- [x] [TUI resume, input, and status](issues/tui-resume-input-and-status.md)
- [x] [Shift-Enter inserts a newline](issues/shift-enter-newline.md)
- [x] [Bounded event channels](issues/bounded-event-channels.md)
- [x] [Cache transcript rendering](issues/cache-transcript-rendering.md)
- [x] [Index session replay](issues/index-session-replay.md)
- [x] [Write tool progress and stalls](issues/write-tool-progress-and-stalls.md)
- [x] [Streaming tool progress](issues/streaming-tool-progress.md)
- [x] [Provider stream error diagnostics](issues/provider-stream-error-diagnostics.md)
- [x] [Task notification presentation](issues/task-notification-presentation.md)
- [x] [Task delegation guidance](issues/task-delegation-guidance.md)
- [x] [TUI content margins and wrapping](issues/tui-content-margins-and-wrapping.md)
- [x] [Discard stale wheel input](issues/discard-stale-wheel-input.md)
- [x] [Accurate tool and subagent activity](issues/accurate-tool-and-subagent-activity.md)
- [x] [Parallel read-only review agents](issues/parallel-read-only-review-agents.md)
- [x] [Extract provider transport](issues/extract-provider-transport.md)
- [x] [Deterministic test harness](issues/deterministic-test-harness.md)
- [x] [Present OpenAI reasoning summaries](issues/present-openai-reasoning-summaries.md)
- [x] [Render Markdown tables](issues/render-markdown-tables.md)
- [x] [Hierarchical transcript activity](issues/hierarchical-transcript-activity.md)
