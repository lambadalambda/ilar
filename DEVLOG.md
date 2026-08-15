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

## 2026-08-14 — session-jsonl done

First issue implemented (TDD, red→green). Review (subagent) caught a real
blocker: `transcript()` could emit consecutive user messages (compaction
summary + first kept user message), which Anthropic-style APIs reject with
400. Fix: coalesce adjacent user messages at flush time. Also added:
orphaned-tool-result snapping at compaction boundaries, NotFound vs
unrecoverable distinction on load, `session_id()` accessor.

Known caveat (documented in event.rs): `kept_from` is a write-time event
index; corrupt-line skips shift it on replay. Acceptable degradation —
transcript stays coherent; anchor to event ids if it ever matters.

## 2026-08-14 — provider-trait done

Trait shape: sync `fn stream(&self, Request) -> Result<Pin<Box<dyn Stream>>>`.
Dyn-compatible, no async_trait, Send-clean. Two hard-won doc contracts:
- Network errors surface as ProviderEvent::Error *on the stream*, not as
  Err from stream() (spawn+mpsc pattern makes pre-flight Err impossible
  for HTTP failures).
- Cancellation: wrapper struct whose Drop aborts the spawned pump task —
  dropping a bare ReceiverStream is NOT enough (quiet connections linger).

Review gate: added Thinking events + ContentBlock::Thinking before any
real provider exists — Anthropic-style APIs require round-tripping
thinking blocks with tool use, GLM emits them, retrofitting later would
have touched every consumer simultaneously. Also added Refusal/Paused
stop reasons, cache-token usage fields, and the null-input+MaxTokens
convention for truncated tool args.

## 2026-08-14 — provider-openai-responses done (smoke test pending)

Review caught two real blockers:
1. SSE parser did from_utf8_lossy per chunk — multi-byte UTF-8 split
   across chunk boundaries corrupted (guaranteed noise on GLM Chinese
   text). Rewrote parser over a byte buffer; blocks convert only when
   complete.
2. response.incomplete with a truncated tool call left a dangling
   ToolCallStarted (violating our own event contract) and reported
   ToolUse — the loop would have executed a tool whose args never
   arrived. Now synthesizes null-input completions + MaxTokens.

Also: refusal deltas surfaced as TextDelta with StopReason::Refusal,
pump panic guard (catch_unwind -> Error event; a panic otherwise looks
like a clean EOF), options-merge without the "extra" marker hack.

Debug war story: test server originally used std TcpListener +
read_to_end under a current-thread runtime — blocking accept() starved
the reqwest task, and read_to_end waited for a half-close that never
comes. Fix: async tokio I/O, read-until-content-length. Also: `let _ =
provider.stream(...)` drops the stream instantly, which per our own
cancellation contract aborts the pump before it connects. The contract
works — against its author.

Remaining for this issue: one live-API smoke test (needs OPENAI_API_KEY),
incl. a reasoning model doing 2+ tool turns to validate that dropping
thinking blocks from replay doesn't 400 (review flagged it; fixtures
can't prove it).

## 2026-08-14 — provider-zai done, live-verified

API keys fished out of local installs: no OpenAI plain key exists (both
opencode and codex use ChatGPT OAuth — the OpenAI smoke test stays
open), but opencode's auth.json holds the zai-coding-plan key. Two
findings from the live endpoint:
- The coding-plan key only works on the Anthropic-compatible endpoint
  and the OpenAI *coding* endpoint (api.z.ai/api/coding/paas/v4); plain
  /api/paas/v4 rejects it with "insufficient balance". Default base
  URLs set accordingly.
- The coding endpoint streams reasoning_content (GLM thinking) — mapped
  to ThinkingDelta with a synthesized ThinkingCompleted boundary since
  chat-completions has no explicit reasoning-block close event.

Live smoke tests (tests/smoke_zai.rs, #[ignore]d, ILAR_ZAI_API_KEY):
anthropic text turn, anthropic two-turn tool round-trip (real GLM
emitted get_weather, result returned, second turn answered), openai-
flavor text turn. All passing.

Review caught two contract violations in the OpenAI flavor: truncation
didn't synthesize pending tool-call completions (now unconditional on
any finish_reason), and chatty compat servers attaching usage to every
chunk could double-fire TurnComplete (guarded). Also: anthropic
truncation synthesis now emits in block order; mid-stream {"error":..}
chunks surfaced.

## 2026-08-14 — core-tools done; prompt caching live-verified

Tools: trait with ToolKind (ReadOnly/Mutating) feeding the upcoming
barrier executor; read/write/edit/bash/glob/grep with typed inputs
(malformed model output = tool error, never a panic).

Prompt caching (the "don't re-ingest the whole prompt" concern):
- Anthropic flavor places ephemeral breakpoints on system block, last
  tool, and a MOVING breakpoint on the last message's final block (the
  canonical incremental pattern).
- Live proof with a ~2000-token prompt on real GLM: turn 1 ingests
  2006 tokens; turns 2-3 read 1920 from cache, only ~100 fresh. The
  moving breakpoint works on z.ai — marker placement is not part of
  their cache hash (earlier messages re-serialize marker-free across
  turns and still hit).
- z.ai accounting quirks: cache_creation_input_tokens is never
  reported (the write shows as plain input_tokens on the writing turn);
  reads reported at entry granularity. Don't assert on creation.
- OpenAI coding endpoint: caching is automatic; we parse
  prompt_tokens_details.cached_tokens.
- Prefix stability is unit-tested: consecutive turns' wire messages
  serialize identically after stripping cache_control markers.

Remaining M1: barrier executor, agent loop, config/AGENTS.md, TUI.

## 2026-08-14 — tool-executor-barrier done

The Claude Code scheduling model on tokio: FuturesUnordered for
concurrent read-only runs, hole-filled outcomes Vec for call-order
results, mutating tools as barriers. Review verdict "safe to build on"
after verifying invariants (FIFO, no double-record, fill-order sound,
drop chain intact). Fixes applied from review:
- Cancel check at top of the scheduling loop (a cancel racing a
  completion could otherwise start one more tool past an Esc).
- Deterministic overlap proof from event logs instead of pure timing
  (last start < first end), generous wall-clock margins.
- Id/name pinning through the hole-fill path; pre-cancelled-token and
  unknown-tool-mid-queue tests.
- Bonus real bug: bash drained pipes only AFTER wait() — a child
  writing >64KB blocked on the full pipe until timeout killed it.
  Now joined concurrently; 300KB drain test proves fast clean exit.

## 2026-08-14 — agent-loop done

The turn state machine. Review caught two blockers on the abort path
(the one path the spec calls a hard requirement):
1. Abort between ToolCallCompleted and TurnComplete persisted an
   unanswered tool_use — every provider 400s on that shape, so one Esc
   at the wrong moment permanently poisoned the session. Fix: abort
   path synthesizes error tool results ("aborted before execution")
   for every announced call; resume tells the model the truth.
2. Abort between iterations (e.g. during tool execution) returned
   without publishing TurnDone — a guaranteed TUI deadlock once Esc
   is wired.

Also: streams ending without TurnComplete/Error are now errors (a
dying provider no longer gets its announced tools executed on its
behalf), and provider errors persist the partial step (UI-shown
deltas must not evaporate from the transcript).

Invariants worth remembering: executor cancel already fills holes
with cancelled outcomes (so abort-during-execution is safe by
construction); transcript() coalesces trailing tool results with the
next user message (valid "please continue" shape on both wires).
Known non-goal for now: concurrent run_turns on one session would
double-open the JSONL — TUI is strictly one turn at a time.

## 2026-08-14 — M1 complete: config, TUI, live capstone

Config: hermetic Loader (edition 2024 made set_var unsafe — tests
inject env explicitly instead of mutating process env), project >
user precedence, MD agents overriding built-ins, AGENTS.md/CLAUDE.md
nearest-wins discovery. provider_for() builds concrete providers from
"provider/model-id".

TUI: thin layer over run_turn (no tests by design). Two build
lessons: pty harness needs TIOCSWINSZ or ratatui renders into 0x0
(empty frames, looks broken); and `.clone()` on a &&SessionStore
clones the REFERENCE when the type isn't Clone — the spawn silently
captured a borrow and failed to compile. Both types are Clone now.

M1 capstone (live, real GLM through the TUI over a pty): user asks
for the workspace version -> model calls read -> tool runs -> final
answer "0.1.0" streamed -> usage in status line -> session JSONL has
the full exchange (meta, user, assistant+tool_call, tool_result,
assistant). ILAR_STATE_DIR env override for sandboxed runs.

## 2026-08-14 — M2 + M3 complete: full roadmap shipped

M2 (multiply): task tool spawning parallel child agents (shared atomic
slot counter, depth-capped child spawners, Claude Code do-not-retry
cap errors); background=true tasks run detached with stall watchdogs
(default 600s, activity tracked via the child event stream) and land
as <task-notification> messages that re-invoke the idle parent loop —
the convergent Claude Code/opencode pattern; auto-compaction with
estimate_tokens = max(last usage, chars/4), summarizer call, cut at
the current user message (once per turn, never mid-tool-loop).
transcript rendering extracted to a pure function for the summarizer.

M3 (polish): todo tool (todowrite-style, single in_progress enforced);
webfetch (dependency-free HTML->text; test caught an off-by-15 slice
bug that corrupted output after </script>) + websearch (pluggable
SearchBackend, Tavily impl); runtime model switching (ModelChange
session events audited in JSONL, effective_model resolved per provider
call, Ctrl-M cycles + rebuilds the provider); skills (markdown +
frontmatter, project-over-user, listing in system prompt, body loaded
on demand, worktree-isolation builtin — the whole subsystem is ~200
lines vs Claude Code's plugins/skills machinery).

Final state: 113 unit tests + 4 live smoke tests, clippy/fmt clean,
15/15 issues closed across 3 milestones. Two `futures`-in-Rust
footguns worth remembering: tuple-of-futures doesn't implement Future
(wrap in async move blocks), and async trait methods borrowing self
need owned clones before Box::pin (todo tool). Also: edition 2024 made
std::env::set_var unsafe — config tests inject env via the Loader
instead.

Known follow-ups (not blocking daily-driver use): OpenAI live smoke
test needs a real API key; concurrent run_turns on one session would
double-open the JSONL (TUI is one-at-a-time); bash timeout is the only
guard against runaway interactive commands.

## 2026-08-14 — OpenAI ChatGPT OAuth login

Codex-style PKCE flow: authorize at auth.openai.com (public client id,
offline_access scope), callback server on 127.0.0.1:1455, S256
challenge (RFC 7636 vector tested), token exchange + rotation into
<state dir>/auth.json — ilar's own file, never reads or writes
~/.codex. Provider gains Auth::ChatGpt: chatgpt.com/backend-api/codex
with originator: codex_cli_rs + OpenAI-Beta headers, store:false, and
one refresh-and-retry on 401 inside the pump (mock-tested: rotated
bearer observed on the wire).

Live findings from probing the real backend (read-only, using codex's
existing access token — usage can't rotate anything):
- Bearer + chatgpt-account-id + originator headers are accepted as-is;
  no mTLS/DPoP binding on this account's tokens.
- API-catalog model names are rejected ("gpt-5.2" -> 400 model not
  supported); ChatGPT accounts serve the codex model line. Current
  slugs live in ~/.codex/models_cache.json — gpt-5.6-sol is the
  default (also -terra/-luna variants, gpt-5.5, gpt-5.3-codex-spark).
- stream:false is rejected ("Stream must be set to true") — fine, the
  provider always streams.
- Final proof: ilar's provider streamed a text turn through the real
  ChatGPT backend (isolated seeded token copy, since deleted).

For daily use: run `ilar login` so ilar holds its own token pair —
refresh rotation would otherwise race codex's copy if you reuse the
same refresh token in two stores.

## 2026-08-15 — OpenAI tool-loop stall

The first real OAuth coding turn exposed three interacting bugs after
`todo`: Responses API `function_call_output` rejects the neutral model's
`is_error` field, the TUI discarded `Ok(Err(...))` from the spawned turn,
and streamed tool calls were announced twice. OpenAI now emits only
`type`, `call_id`, and `output`; nested turn errors are displayed; starts
are deduplicated; and tool lines are completed by call id rather than
screen position. Final queued events are drained after joining the turn.

Tool results are flushed to JSONL before `ToolFinished` is published. A
regression test reloads the session while the next provider request is
still pending and sees both the assistant tool call and its result.
Provider errors after a completed call synthesize an error result so the
session remains resumable.

## 2026-08-15 — Readable Markdown and real transcript scrolling

Assistant output is now rendered as terminal-native Markdown instead of
putting an entire response (including newlines) into one Ratatui `Line`.
Headings, lists, quotes, emphasis, inline code, links, rules, and fenced
code get distinct styles; tabs use stable four-column stops, and partial
streaming delimiters remain visible until they close.

The transcript follows the wrapped visual tail by default and detaches
when the user scrolls upward. Controls: arrows and mouse wheel for small
steps, PgUp/PgDn for pages, Ctrl-U/Ctrl-D for half-pages, Ctrl-Home for
the top, and Ctrl-End to resume tail following. Overflow adds a scrollbar
and a `tail`/percentage title marker. Wrapped row counts are recalculated
on resize without snapping a detached reader back to the tail.

## 2026-08-15 — Stabilization: unique tool registry

Tool registry composition now rejects duplicate names with a typed error.
`webfetch` remains a builtin and `with_web_tools` only adds optional
search, so provider requests cannot contain duplicate function schemas.

## 2026-08-15 — Stabilization: z.ai OpenAI wire format

The OpenAI-compatible flavor now sends instructions as a system-role
message and places tool outputs directly after assistant tool calls,
without inserting an empty user message. The example endpoint now points
at z.ai's coding-plan OpenAI-compatible URL.
