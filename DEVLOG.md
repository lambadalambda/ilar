# DEVLOG

## 2026-08-14 — Project genesis

### Research: how Claude Code and opencode actually work

Did binary surgery on Claude Code 2.1.220 (Bun-compiled Mach-O, ~257MB,
JS bundle carved out and mined) and read the opencode source
(`anomalyco/opencode` dev branch). Findings that shaped ilar's design:

**Claude Code:**
- One process, one JS event loop. Subagents are *not* processes or workers;
  they are recursive async-generator query loops.
- Fan-out in a `StreamingToolExecutor`: tool_use blocks enqueue as they
  stream in. Each tool declares `isConcurrencySafe()`. A queued tool may
  start if nothing is executing OR it and all executing tools are
  concurrency-safe. Mutating tools (Edit/Bash) form a barrier. Results are
  drained in tool order (deterministic), execution is concurrent.
- Subagent caps are plain counters in a taskRegistry
  (`takeConcurrencySlot`): 20 concurrent (`CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS`),
  200/session, plus a spawn-depth cap. Over cap = soft tool error, no retry.
- Background agents: fire-and-forget + stall watchdog (600s). Completion
  enqueues a *synthetic user message* (`mode: "task-notification"`,
  `priority: "next"`) into the owner's queue, which re-invokes the parent.
  The "messaging system" is an in-process priority queue. Real IPC only for
  bash subprocesses and MCP servers.

**opencode:**
- Effect.ts fibers instead of promises. Tool calls dispatched as detached
  fibers; results land out-of-order in an unbounded queue; stream ends when
  the FiberSet settles.
- **No concurrency barrier** — everything in a step runs concurrently and it
  trusts model behavior. We adopt Claude Code's barrier instead: it is
  cheap to implement and prevents Edit/Bash races.
- Subagents = real child sessions (DB rows, inspectable in TUI). Depth cap
  default 1, no concurrency cap.
- Background agents exist only behind
  `OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS`; completion injects a
  synthetic text part into the parent session. Same convergent pattern.

**Convergent architecture (both projects, independently):**
one event loop, fire-and-forget children, synthetic user message on
completion. No message broker anywhere. ilar copies this shape with
type-safe channels.

### Design decisions

- Rust workspace, two crates: `ilar` (core, pure) + `ilar-tui` (frontend).
  Core purity keeps a future one-shot CLI / server mode trivial.
- Event bus: `tokio::sync::mpsc<LoopEvent>` per agent; subagents as
  `JoinSet` tasks (structured concurrency, cancel-safe).
- Providers: trait `Provider` with `stream(request) -> EventStream`.
  Implementations: OpenAI Responses API, z.ai Anthropic-compatible,
  z.ai OpenAI-compatible. Mock provider for TDD.
- Sessions: append-only JSONL under `~/.local/state/ilar/sessions/`.
  One file per session, each line an event (message, tool_call, tool_result,
  compaction). Resume = replay file.
- Tools: trait with `kind() -> ToolKind { ReadOnly, Mutating }`. Executor
  adopts the barrier scheduling model.
- Compaction: when transcript nears context limit, summarize older turns
  into a marker event, continue session.
- No permissions. Sandbox is the boundary. (Deliberate: reduces scope by a
  whole subsystem.)
- Skills: markdown + frontmatter, injected on demand. Git-worktree
  isolation for subagents is a *skill*, not core.
- License: Unlicense.

### Constraints / preferences (from requirements interview)

- Personal tool, maybe OSS later. TUI-first. Esc = full abort (stream +
  running tools, best effort).
- TOML config, markdown agent definitions (opencode style).
- AGENTS.md / CLAUDE.md detection + cwd as project root.
- Per-agent model in config + runtime switching.
- Testing: TDD for core (loop, tools, providers via mock SSE), skip TUI.
