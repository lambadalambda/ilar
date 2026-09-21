# Three gateways, side by side — 2026-09-21

`ilar-gateway` against the two other chat-gateway agents checked out
on this machine: `picoclaw` (Go, `~/repos/picoclaw`) and `hermes-agent`
(Python, `~/repos/hermes-agent`, Nous Research, v0.16). Read from the
source, not the READMEs. The point is not to match either — they are
different weight classes — but to see which of their choices answer a
need we have and have not met.

## Weight class

| | picoclaw | hermes | ilar-gateway |
|---|---|---|---|
| language | Go | Python (+TS, Rust) | Rust |
| size | ~25k LOC (+18k tests) | ~530k LOC (+567k tests) | ~10k gateway on an 80k core |
| tests | 561 | ~28,000 | 97 gateway, 1,876 workspace |
| CI | none | 18 workflows | none |
| deployment | Docker, runit, Proxmox LXC scripts | Docker, Nix, Homebrew, systemd/launchd/s6, Electron | systemd user unit, install script |

hermes is a platform with a gateway in it. picoclaw is a small daemon
with a lot of channels. We are a thin gateway on a large core, and the
core carries most of what the others have to build twice.

## Where we are ahead

Worth stating, because the gap list below is long and would otherwise
read as "behind everywhere".

- **Delivery is honest end to end.** A message the channel refused
  reaches both the chat and the model's next prompt. A subagent's
  report survives a restart through the durable outbox. A scheduled
  turn can only speak through the `message` tool. Neither of the
  others closes those loops; picoclaw logs and drops.
- **Mid-turn steering** as a first-class thing, with an ack when the
  status line is not there to show it. hermes has `/steer`; picoclaw
  interrupts and restarts the run.
- **The status line protocol** — one edited message per seat, with
  ownership, throttling and shutdown cleanup. hermes streams drafts
  (better on Telegram); picoclaw has typing indicators only.
- **Secrets never enter context**, per-use grants answered from the
  chat, the master password retracted from the chat even when
  mistyped. picoclaw keeps keys in a 0600 JSON file; hermes redacts
  logs well but has no consent flow for a stored secret.
- **Memory that a review writes**, with an approval queue, a weekly
  review, skill archival by disuse, and the room/private split that
  keeps a group away from the person's memory. hermes has the same
  shape (background review, curator); picoclaw extracts at compaction
  only.
- **Prompt-cache-aware placement** of the timestamp and the review's
  timing. hermes does cache breakpoints; picoclaw sets the Anthropic
  beta flag.
- **Tool policy by construction** — a denied tool is absent from the
  model's list and narrowed into every subagent definition. picoclaw's
  policy is a runtime check; hermes has toolsets per surface, which is
  the same idea with more machinery.
- **Per-channel outbound lanes** (since this morning). hermes has
  per-adapter retry; picoclaw's dispatcher is one queue.

## Where we are missing things

Grouped by how much they matter for the way the gateway is actually
used here: one person, one box, Delta Chat.

### 1. Channels — the one gap that is structural

| | picoclaw | hermes | us |
|---|---|---|---|
| platforms | 8 (Telegram, Discord, Slack, DeltaChat, WhatsApp, Feishu, DingTalk, QQ) | 22 built-in + 10 plugins (incl. Matrix, Signal, IMAP email, SMS, iMessage, IRC, webhook, API server) | 1 (Delta Chat) |
| streaming to chat | no | drafts/edits, per-platform cadence | no (status line only) |
| voice in | Groq Whisper (TG/Discord/Slack) | STT + TTS, many providers | no |
| reactions | Slack, DeltaChat (in+out) | 9 platforms in+out | ack emoji out only |
| edit/delete own message | declared, unused | 9 platforms | status line only |
| multiple attachments in | yes | yes | one per message |
| HTTP API / webhook receiver | no | OpenAI-compatible server, HMAC webhooks, MCP server | no HTTP at all |

The `Channel` trait is small and `channels_from` is a match on a
string, so a second channel is a bounded piece of work. Telegram or
Matrix would be the natural first. Everything else in this table is
second-order until there is a channel that can do it.

**Voice notes** are the one media gap that bites on Delta Chat today:
a voice message arrives as `(file attached: …)`.

### 2. What the person can ask for

hermes has ~80 slash commands, picoclaw none, we have 14. Most of
hermes's are noise for us, but these are things we already *have* in
the core or the state and simply do not expose in a chat:

| command | what it would show | where it lives today |
|---|---|---|
| `/status` | model, context %, running tools/tasks, pending asks | TUI status line |
| `/cost` or `/usage` | this session's spend and cache rate | `model.rs::cost`, TUI only |
| `/cron` | list / remove jobs | only the model, via the tool |
| `/tasks` | subagents running and held results | `tasks` tool, model only |
| `/sessions` | search across sessions, resume one | `recall::search_sessions`, TUI |
| `/rewind`, `/fork` | checkpoints are *taken* for gateway turns | `rewind.rs`, TUI |
| `/skills`, `/memory` | list, and the pending approvals | `/pending` covers approvals only |
| `/restart` | drain and exit for systemd | none |
| `/whoami` | am I allowlisted, which tier | none |

These are cheap individually — each is a read of state that exists —
and together they are the difference between "a chat" and "a console".

### 3. Permission model for a box anyone can message

`policy.rs` says it itself: "the sandbox is the permission system is
right for a terminal and wrong for a box anyone can message." Both
others have a second layer we lack:

- **Dangerous-command approval.** hermes: pattern-matched, `once /
  session / always / deny`, "always" persisted to an allowlist, an
  auxiliary model can auto-approve low risk. picoclaw: `UNSAFE_OK`
  with a TTL, plus an `exec` denylist. We have `safe_mode` (all or
  nothing) and the secret grant flow — but a plain `rm -rf` needs no
  secret and asks nobody.
- **Who may talk.** All three have sender allowlists. hermes adds DM
  pairing (a code the operator approves, with lockout) and tiered
  slash access (`allow_admin_from`, per-user allowed commands). We
  have one tier: allowed or ignored.
- **Per-chat policy.** hermes: toolsets per platform, per cron job,
  per webhook route. We: one process-wide policy, plus room vs
  private.
- **Sandbox.** hermes runs tools in six backends including Docker,
  Modal, Daytona. We have `kernel-sandbox-for-tool-processes` open
  and the README's instruction to run inside one. picoclaw has none
  either.
- **Injection scanning.** hermes scans context files, tool results,
  memory writes, skill installs, cron prompts and MCP configs against
  a shared pattern library; its webhook toolset is read-only because
  payloads are untrusted. We refuse project instructions, withhold
  paths from rooms, and indent grant commands — targeted, not general.
  Whether pattern scanning is worth its false positives is a
  judgement; the *toolset-per-source* idea (a notify from a script
  gets fewer tools than the person) is not, and we have the seam for
  it (`is_script()`).

### 4. Providers

| | picoclaw | hermes | us |
|---|---|---|---|
| providers | 8 | 30+ | 4 + custom |
| Anthropic wire | yes | yes | open issue |
| fallback chain | ordered `fallback_models` | per-error-class fallback, credential pools | none |
| cost tracking | tokens to JSONL, `usage` CLI | pricing, credits, insights | in core, not in gateway |

A fallback model is the one of these an always-on assistant actually
needs: a provider outage today means the chat gets a failure line and
nothing else.

### 5. Automation

We have cron (tool-driven), heartbeat, file inbox, weekly review. The
others add:

- **Script-gated jobs** (hermes): a prerun script whose output decides
  whether the model runs at all — a job that polls a URL costs zero
  tokens until something changes. Our `ilar-gateway notify` covers
  half of this from the outside; the other half (the job owns the
  script) is cheap to add to `cron.json`.
- **HTTP webhook receiver** (hermes, HMAC per route, idempotency,
  rate limit). We chose no HTTP; the file inbox is the equivalent for
  scripts on the same box. Anything remote has to ssh in to drop a
  file. That is a choice, but it should be written down as one.
- **Automation blueprints and suggestions** (hermes): catalogued,
  consent-first "want me to set up a morning briefing?" flows. Nice;
  not a need.
- **`/goal`** exists in our TUI and not in the gateway. hermes runs it
  on every surface.
- **Heartbeat** — picoclaw's is unconfigurable (30 min, always on);
  ours is configurable and off by default. Fine.

### 6. Model-facing tools

The core is strong; these are what the others give the model that we
do not:

- **A session-search tool** (hermes `session_search`, FTS5 across all
  past sessions). Our `history` tool searches only the current log,
  and `recall::search_sessions` exists in the core for the TUI.
- **`clarify` / `question`** — deliberately off in the gateway
  ("nobody sits at a channel to fill in a form"). hermes renders the
  choices as buttons where the platform has them. Worth revisiting
  once there is a platform with buttons.
- **Browser tools** (hermes, 12 of them) and **`execute_code`** (the
  model writes Python that calls tools over RPC, collapsing a chain
  into one turn). Neither is a gateway concern; both are core gaps
  if wanted.
- **STT / TTS / video** — see channels.
- **MCP client** — we do it through a skill and an external CLI;
  hermes has native stdio/HTTP/SSE with deferred schemas behind
  `tool_search`. picoclaw has none.

### 7. Ops

| | picoclaw | hermes | us |
|---|---|---|---|
| logging | JSON lines + human, levels, trace ids | rotating files per component, redacting formatter, `hermes logs` | `eprintln!` with a timestamp |
| health | none (compose check on the bridge) | `/health`, `/health/detailed`, memory monitor | none |
| config reload | no | `/reload`, `/reload-skills`, `/reload-mcp` | no |
| restart | supervisor | `/restart` drains, exit 75 | SIGTERM |
| diagnostics | `status` CLI | `doctor`, `/debug` upload | `prompt` CLI |

The log is the gateway's "only voice that is not somebody's chat", and
it has no levels and no structure. journald gives rotation and search;
it does not give a `--level`. A `/restart` that drains is small and
useful with systemd.

### 8. Configuration and docs

- `ilar.toml.example` has **no `[gateway]` or `[channels]` section**.
  Everything is in `docs/gateway.md`, which is good, but the example
  file is where a person looks first.
- hermes's per-platform display overrides and `platform_hint` (a line
  of prompt per platform: "you are on IRC, no markdown") map onto our
  `constraints()` — we have the mechanism, one channel's worth of it.

### 9. Things the others do that we should *not* copy

- picoclaw's "all replies must go through the message tool, plain
  assistant text is suppressed" — we have the same rule for scheduled
  turns and deliberately not for the person's turns.
- hermes's plugin system with 27 hooks, and its profiles-as-separate-
  homes. Our agents-as-markdown and one home per gateway are the
  right size for one box.
- picoclaw's `tools.safeguards.disabled = true` in the shipped example
  config. Our `safe_mode` defaults the other way.
- hermes's kanban multi-agent board. Our task/outbox model is the
  simpler answer to the same need at our scale.

## If I had to pick five

In the order I would take them, each its own issue:

1. **A second channel** — Telegram (largest reach, has buttons,
   drafts, reactions) or Matrix (self-hosted, matches the Delta Chat
   ethic). Everything in §1 hangs off this.
2. **Dangerous-command approval** with `once / session / always`,
   reusing the grant flow's ask-and-answer machinery — the grant code
   already knows how to show a command safely and wait ten minutes.
3. **A fallback model** for the gateway: one config key, tried when
   the provider's retry budget is spent.
4. **The console commands** — `/status`, `/cost`, `/cron`, `/tasks`,
   `/sessions`, `/restart` — all reads of state that exists.
5. **Structured logging with levels**, so `journalctl -p warning` means
   something, and `[gateway]`/`[channels]` in the example config.

Voice-note transcription and a `session_search` tool for the model are
the next two.

## Sources

Inventories were taken from the code on 2026-09-21: picoclaw at its
last commit (2026-05-27), hermes-agent 0.16.0, ilar at `90a876e`.
Feature claims are from reading implementations, not documentation;
where a feature was declared but unused (picoclaw's Telegram
edit/delete) it is counted as absent.
