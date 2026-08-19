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

## 2026-08-15 — Stabilization: serialized notification turns

Notification bursts now stay queued and launch one turn at a time. The
active JoinHandle, rather than an early UI `TurnDone` event, owns the turn
until join cleanup, preventing event-channel, cancellation-token, and
handle replacement races. Parent-session routing remains tracked in the
open notification issue.

## 2026-08-15 — Stabilization: provider/model boundary

Concrete providers now reject models with a mismatched provider prefix
before spawning network work. This converts stale routing mistakes into
clear preflight errors while the broader provider-router issue remains
open for resume, switching, subagents, and compaction.

## 2026-08-15 — Stabilization: robust Bash execution

Bash drains stdout and stderr concurrently into bounded byte buffers,
decodes arbitrary output lossily, and truncates only at UTF-8 boundaries.
On Unix each command runs in a dedicated process group; timeout or future
cancellation terminates descendants. Timeout errors
retain bounded partial output, and signal exits have explicit diagnostics.

## 2026-08-15 — Stabilization: validated session identifiers

Session file paths now derive only from canonical lowercase hyphenated
UUIDs. Invalid CLI or model-supplied task IDs fail with `InvalidInput`
before any filesystem lookup, closing the path-traversal route while the
writer-lease issue remains open.

## 2026-08-15 — Stabilization: session writer lease

Every agent turn now holds a nonblocking OS-backed session writer lease
from before the user append through provider/tool completion. Concurrent
turns fail before mutation, cancellation releases ownership, and read-only
loads remain available. Direct compaction acquires the same lease while
turn-internal compaction reuses existing ownership. Tail-recovery behavior
remains tracked before the lease issue can be archived.

## 2026-08-15 — Stabilization: torn-tail recovery

Read-only session inspection parses only newline-terminated records and
never repairs a file. A leased writer truncates an unterminated or invalid
UTF-8 final tail to the last complete record before appending. Malformed
newline-terminated records now reject the session as middle corruption
instead of being silently skipped.

## 2026-08-15 — Stabilization: crash-safe replay

Session replay now requires one leading metadata event whose identity matches
the filename, unique event and tool-call IDs, and exact tool-call/result
pairing. Read-only inspection leaves trailing unanswered calls untouched;
leased writer recovery persists synthetic error results for them exactly once.
Invalid semantic state is rejected before any torn-tail repair mutates the log.

## 2026-08-15 — Stabilization: typed session identity

All session and lock paths are now derived internally from a canonical,
validated `SessionId`. Cross-process tests prove actionable nonblocking
contention and OS lock release after forced process exit; the test helper is
isolated and reaped on timeout or panic. Platform-specific `fs2` contention
errors are normalized to `WouldBlock`.

## 2026-08-15 — Stabilization: provider/model lifecycle

Each writer-owned turn now captures the persisted effective model and resolves
its matching provider exactly once before appending user input. The same pair
drives compaction and every tool-loop step. Resume defaults to persisted agent
and model state, explicit CLI model selection wins and persists first, and
subagents inherit the parent model unless their agent overrides it. Nested
background tasks are rejected until parent-specific notification contexts are
supported.

## 2026-08-15 — Stabilization: ordered reasoning state

Assistant content now persists in exact stream order across thinking, text,
opaque reasoning, and tool calls. Signed thinking runs remain independent;
unsigned or incomplete thinking is retained only as non-replayed diagnostics.
Stateless OpenAI requests preserve encrypted reasoning items before function
continuations. Incomplete tool calls are never executed, receive synthetic
errors, and replay with protocol-valid placeholder arguments.

## 2026-08-15 — Stabilization: notification routing

Background completions now enter one FIFO and execute only when no foreground
or routed turn owns the TUI lifecycle. Nested completions run their declared
parent with its persisted agent, model, depth, and registry, then propagate one
success or error upward. Busy parents wait without losing work; cancellation
requeues undelivered notifications in a paused state, while delivered aborts
propagate explicitly. Nested detached handles share root cancellation ownership.

## 2026-08-15 — Stabilization: atomic replacement

OAuth credentials and source-file write/edit operations now share one
crash-durable replacement primitive. On Unix, temporary creation, destination
inspection, publication, cleanup, and directory sync are bound to one
no-follow directory descriptor. Temps are born `0600`, final modes are applied
after writing, parent swaps and symlink destinations are rejected, and
post-publication durability failures are reported without unsafe cleanup.
Other platforms fail closed until equivalent handle-relative guarantees exist.

## 2026-08-15 — Stabilization: secure OAuth storage

OAuth store reads now distinguish absence from malformed, unreadable, or
symlinked credentials. All token writes and refresh rotations share an
OS-backed lock, recheck state after lock acquisition, and retain ownership
through cancellation-safe blocking persistence. Token responses and localhost
callback requests are bounded and timed; callback handling ignores spurious
connections, percent-decodes values, and reports OAuth denial responses.

## 2026-08-15 — Stabilization: bounded file tools

Read now streams requested line windows from files larger than the output cap
and distinguishes empty files from offsets beyond EOF. Read, grep, and glob
filesystem work runs off async workers with cooperative cancellation on future
drop. Grep bounds each file prefix, rendered line, match count, and total
output; glob checks cancellation per traversed entry and stops collecting at
its cap. Atomic write/edit publication and mode preservation are shared with
the previously landed replacement primitive.

## 2026-08-15 — Background Bash jobs

Bash can now opt into detached execution with `run_in_background`, returning a
stable job ID immediately and delivering one completion, failure, timeout, or
cancellation notification through the existing parent-turn queue. Background
jobs use a configurable 10-minute default (`subagents.background_tool_timeout_ms`)
with per-call `timeout_ms` overrides, retain workspace exclusion for their full
run, inherit root cancellation through nested agents, and are cancelled and
joined during shutdown. Bash also terminates remaining process-group children
when the shell exits.

## 2026-08-15 — TUI tool details and telemetry

Tool rows now receive bounded, secret-redacted argument summaries from the
agent loop and render them as muted, grapheme-safe, single-line text. The
always-visible status strip reports lifecycle state, effective model, working
directory, normalized context usage/limit, and percentage with responsive
layouts down to narrow terminals. Thinking, responding, and tool activity are
animated in the transcript without persisting synthetic content. Provider
usage now has versioned cache-accounting semantics; legacy sessions and resumed
transcripts use visibly approximate estimates.

## 2026-08-15 — Model catalog and picker

Provider discovery now uses a maintained models.dev snapshot for active
tool-capable OpenAI and z.ai models, including model-specific context, input,
and output limits. ChatGPT OAuth remains restricted to model slugs verified on
the Codex backend, while API-key and z.ai Coding Plan inventories follow their
effective transport configuration. The TUI replaces model cycling with a
searchable keyboard modal opened by Ctrl-X M or F2; selection is persisted
before adoption, background notifications wait behind the modal, and narrow
layouts retain selectable rows and inline errors.

## 2026-08-15 — Conservative GPT-5.6 context defaults

GPT-5.6 Sol, Terra, and Luna now use Codex's 272,000-token working context for
telemetry and compaction while retaining the models.dev 1,050,000-token value
as explicit maximum metadata. This separates safe runtime defaults from
provider capability and leaves a clean bound for future context configuration.

## 2026-08-15 — Denser Markdown and visible input cursor

The TUI now exposes the terminal's native blinking cursor at the prompt and
model search, with grapheme-safe tail views for long values. Markdown blank-line
runs collapse to one interior separator row without adding leading or trailing
space, and assistant content is style-preserving hard-wrapped inside its label
margin so every visual line stays aligned without changing fenced-code
whitespace.

## 2026-08-15 — Workspace-aware tool scheduling

Tool ordering and workspace effects are now independent capabilities. Mutable
child turns hold a checkout-wide lease for their full lifetime, enforced
read-only agents receive no shell, edit, write, or delegation tools, and todo
updates remain ordered barriers without pretending to read the workspace.

Task calls may route to a registered sibling Git worktree with structured
`workspace` metadata. Canonical checkout IDs key a shared lock registry, so
same-checkout mutations serialize while distinct worktrees overlap. Child
sessions persist their validated cwd and isolation; resumes require the same
explicit worktree when changing workspace and may inherit an immediate parent's
validated location. Routed notifications restore each ancestry transition,
and stale worktrees are rejected again after lease waits. Routing is
cooperative scheduling, not a filesystem sandbox.

## 2026-08-15 — Correct, cancellable compaction

Every root, child, background, and routed turn now shares the configured loop
settings. Compaction estimates only active post-boundary context while counting
the system prompt and tool definitions used by the real request; startup and
model-switch telemetry use the same estimator.

Summaries are persisted only after an explicit `EndTurn`. Partial EOF, refusal,
pause, truncation, tool-use, provider errors, and empty output leave no
compaction marker. Cancellation is checked before the summary call, while its
stream is pending, and immediately before persistence, allowing Escape to
return the turn as aborted without committing a partial summary.

## 2026-08-16 — Hardened provider protocol handling

Provider streams now fail closed on malformed JSON, missing identifiers,
duplicate or contradictory lifecycle events, invalid tool arguments, and
unterminated or oversized SSE events. Reserved request fields are rejected
before network I/O, and bounded HTTP error bodies redact structured,
plaintext, configured, and truncation-boundary credentials.

The agent loop enforces explicit tool start/completion ordering, permits null
arguments only for explicit token truncation, and never invokes custom tools
with incomplete input. Anthropic pauses have a finite retry budget independent
of normal tool iterations; exact streamed assistant content is replayed for
continuation and persisted in provider-specific replay blocks once the resumed
turn completes. This preserves server-tool ordering through later client tool
results without duplicating visible neutral content.

## 2026-08-16 — Bounded, SSRF-safe web tools

Web fetch and Tavily search now use explicit connect and total timeouts, disable
environment proxies, and stream response bodies under hard byte ceilings.
Fetch validates literal and every DNS-resolved address, rejects private,
loopback, link-local, metadata, and known IP translation ranges, and applies the
same policy to redirects. Tavily redirects are disabled so its body-carried API
key cannot be replayed to another origin.

The HTML converter now scans Unicode safely, handles quoted attributes and raw
script/style content, and preserves block boundaries with single-buffer text
normalization. Search queries, backend duration, result count, JSON size, hit
fields, errors, and final output are bounded; the public limit is documented
and clamped to 1–20 results. Fetch errors strip reqwest URLs and retain only a
bounded origin label so signed paths and queries are not persisted.

## 2026-08-17 — Strict, layered configuration diagnostics

Config files now merge nested provider, compaction, and subagent fields instead
of replacing whole sections. Only missing files are ignored; read, UTF-8, parse,
and semantic errors retain their source paths. Loader-injected config and state
directories now drive OAuth, sessions, agents, and skills consistently, with an
explicit OpenAI `api_key` mode available to reset inherited ChatGPT auth.

Agent and skill discovery is deterministic across user and project directories.
Their shared frontmatter parser accepts BOM and CRLF input, requires exact
delimiter lines, and reports malformed definitions rather than dropping them.
Checked-in config and agent examples are parsed by tests so documentation cannot
silently drift from supported fields.

## 2026-08-17 — Resumable, editable TUI sessions

Resumed sessions now rebuild their visible transcript before entering raw mode,
including compaction summaries, redacted tool details, completed tool states,
model switches, todos, and the latest meaningful token usage. Persisted agent
and model selection remain validated before terminal initialization.

The prompt is now a grapheme-safe multiline editor with cursor movement,
in-place deletion, bracketed paste, vertical line navigation, and explicit
Enter-to-send versus Ctrl-J-to-insert-newline bindings. Input expands to show up
to six lines and reports the current line, while idle status prioritizes model
and latest usage over lower-value path detail at constrained widths.

Transcript rows are wrapped and sliced with `usize` before Ratatui rendering,
removing the Paragraph `u16` scroll ceiling. A bounded-row fast path keeps the
65k-row tail regression responsive; broader transcript caching remains a
separate stabilization issue.

## 2026-08-18 — Shared provider transport

OpenAI and z.ai now share one private transport shell for bounded HTTP errors,
SSE parsing, terminal-event cutoff, panic conversion, and abort-on-drop task
ownership. Provider modules still own request construction, authentication and
wire-event mapping; in particular, OpenAI's ChatGPT token refresh remains
outside the transport abstraction. Direct shell tests cover send failure,
panic conversion, terminal SSE handling, and prompt cancellation on drop.

## 2026-08-18 — Deterministic provider tests

`MockProvider` now consumes each scripted turn exactly once and reports script
exhaustion from `Provider::stream`, making accidental extra calls fail at their
source. Intentional loop tests opt into `MockProvider::repeating` explicitly.
Provider fixture tests validate required SSE termination without repairing
tracked files; the full workspace suite passes from a read-only source checkout
with a separate writable Cargo target directory.

## 2026-08-18 — Visible OpenAI reasoning summaries

Reasoning-capable OpenAI Responses requests now ask for automatic public
summaries. Their `reasoning_summary_text` stream is validated and persisted as a
display-only content block, while the completed encrypted reasoning item remains
the sole replay input. The TUI extracts the provider's leading Markdown heading
and renders it as `Thinking: <topic>` while streaming and `Thought: <topic>`
after completion; private thinking and unsigned diagnostics remain hidden.

## 2026-08-19 — OpenAI prompt-cache routing diagnostics

OpenAI requests now carry the session UUID as a provider-neutral cache affinity
key. The documented API-key Responses endpoint maps it to `prompt_cache_key`;
custom endpoints omit it by default. ChatGPT OAuth deliberately omits the
undocumented field after controlled keyed samples accepted it but reported
0/0/0 and 0/6912/0 cached tokens, failing to demonstrate stable affinity. An
automatic-cache control with three byte-identical 8k token ChatGPT requests also
reported 0/6912/0. This confirms that zero/high oscillation can be backend
routing, not local prefix mutation.

Regression tests pin the stable serialized model, instructions, tools, reasoning
options, prior-input prefix, and cache key across consecutive requests. OpenAI
usage parsing accepts both Responses `input_tokens_details.cached_tokens` and
Chat Completions `prompt_tokens_details.cached_tokens` shapes. The TUI now labels
cache reads and writes separately for the latest request rather than implying a
cumulative session counter.

## 2026-08-19 — Previewable TUI themes

The TUI now offers Terminal Adaptive, Carbon, Parchment, Frost, and High
Contrast themes through `F3`, `Ctrl-X T`, and the command palette. Picker
navigation transforms the rendered buffer immediately, Escape restores the
saved theme, and Enter confirms it without invalidating semantic transcript
caches or threading palette state through every renderer.

Themes are a user-scoped preference so project configuration cannot make a
successful in-app save disappear after restart. Confirmation updates the user
TOML with a syntax-aware, comment-preserving editor, retains CRLF line endings,
retries concurrent changes, and publishes through the existing atomic-file
path. Modal handling is centralized so queued notifications, paste, and mouse
input cannot leak through the picker.

## 2026-08-19 — Bounded project context discovery

Project instructions no longer walk arbitrary ancestor directories. Root and
subagent prompts combine `AGENTS.md` (or `CLAUDE.md` as a fallback) from the
resolved user config directory and exact runtime working directory, with local
instructions last. Non-missing read failures are surfaced instead of silently
dropping policy or falling through to a legacy file.

The user config directory is carried through nested, isolated, and routed
subagent runtimes. Context is loaded before fresh child-session creation, and
routed nested failures propagate to the grandparent rather than losing a
background completion. The README now provides the corresponding complete
configuration, environment, custom-agent, and skill reference.

## 2026-08-19 — websearch works out of the box (keyless Exa)

Websearch previously registered only with `ILAR_TAVILY_API_KEY` set, so a
fresh install silently had no search. Investigated how opencode ships OOB
search: it POSTs a bare JSON-RPC `tools/call` to the hosted Exa and
Parallel.ai MCP endpoints, keyless by default, and A/B-splits sessions
between the two providers by session-ID checksum.

Adopted the Exa half: new `ExaBackend` calls `https://mcp.exa.ai/mcp`
(`web_search_exa`), parses both direct-JSON and SSE `data:` framings, and
converts the text payload (`Title:`/`URL:` blocks separated by `---`) into
structured `SearchHit`s, with a raw-text single-hit fallback so results are
never dropped. `with_web_tools()` now always registers websearch: Tavily
when its key is set, otherwise Exa (optionally authenticated via
`ILAR_EXA_API_KEY` as an `exaApiKey` query parameter, same as opencode).
Keyless access is best-effort on Exa's side — README tells users to bring
their own key. Live-verified via an `#[ignore]`d smoke test
(`cargo test -p ilar exa_live -- --ignored`).

## 2026-08-19 — Milestone 4: daily-driver UX batch

Eleven-issue batch making the TUI comfortable for daily work, worked
issue-by-issue with per-feature commits and subagent reviews on the two
biggest changes:

- **Edit diffs**: LCS line diff (dependency-free, bounded 400 lines /
  256 KiB) replaces raw old/new JSON in edit tool rows, themed ± colors,
  across live/child/restore paths. Review caught a byte-cap gap and a
  changeless-diff fallback suppression; both fixed.
- **Session resume**: `SessionStore::list()` head-scans JSONL for
  (id, title, mtime); `--continue` resumes the latest; a palette picker
  switches sessions in-app by restarting the whole runtime (main is now
  a session loop) for full agent/model/prompt fidelity. Switch validates
  the target and refuses during turns/background jobs/drafts.
- **Prompt history**: persisted JSONL ring (1000 entries), Up/Down
  recall with readline draft stash.
- **Usage + cost**: models.dev pricing table in core (list prices,
  snapshot-dated); per-step accrual with per-event-model pricing on
  restore; Σ tokens + $ in the status line, breakdown via palette.
  Unpriced models poison dollars, never guess.
- **Help overlay** (F1/?), **readline chords** (^A/^E/^K/^U/^W, Alt-B/F),
  **skill triggers** (frontmatter now feeds prompt cues) + `/skill`
  invocation with a `/` picker, **per-agent `tools:` allowlists**
  (load-time validation, intersection with read-only), built-in
  **mcp-via-cli skill** (decision: no core MCP client), and hand-rolled
  **fence syntax highlighting** (six language families, no syntect).
- Todo narrow-terminal fallback issue closed as already implemented
  (border-chrome summary line predates the issue).

Caveats: session switching drops background-job tracking (spawner is
shut down like on quit); mcp-via-cli verified against upstream docs only
(sandbox blocked installing the CLI); the AppExit::Switch path has no
automated test — smoke-test manually.
