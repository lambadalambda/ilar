# ilar-gateway

ilar as an always-on assistant: a process that listens on messaging
channels, runs each chat as its own ilar session on the library
runtime, and sends the answer back. Milestone 21 in `meta/issues.md`;
this page grows with it.

```sh
ilar-gateway                       # listen on the configured channels
ilar-gateway notify "build green"  # a message from a script, to the last active chat
ilar-gateway notify --to deltachat:12 --source ci "…"
ilar-gateway invite                # the Delta Chat invite link to add the bot with
ilar-gateway prompt                # the system prompt a private chat gets; --group for a room
```

## Running it

`ilar-gateway` runs in the foreground and logs to stderr; Ctrl-C stops
it, waiting a few seconds for turns in flight. `scripts/install.sh`
installs it next to `ilar`. On a systemd machine run it as a user
service, from `scripts/ilar-gateway.service`:

```sh
cp scripts/ilar-gateway.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now ilar-gateway
loginctl enable-linger "$USER"      # keep it up when you are logged out
journalctl --user -u ilar-gateway -f
```

It restarts on failure. After editing `ilar.toml` or `SOUL.md`,
`systemctl --user restart ilar-gateway`: configuration is read at
start, and a chat's prompt when its session opens. The last active
chat is told on the way: "⏹ ilar-gateway stopping", then
"▶ ilar-gateway 0.2.0 (f3cd7a7) started · default model …" with the commit
the binary was built from, so a deploy is visible where you are
looking. `gateway.announce = false` keeps it quiet. Delta Chat's
account survives restarts; the invite is logged at each start and
printed by `ilar-gateway invite`.

## Configuration

Both tables live in the user's `ilar.toml` only; a project file that
declares them is warned about and ignored.

| Key | Default | Meaning |
|---|---|---|
| `gateway.home` | `<state dir>/gateway` | The assistant's home; see below. |
| `gateway.agent` | the core's default | The agent every chat runs as. |
| `gateway.model` | `general.model` | `provider/model` a fresh chat starts on, unless `/model … --save` saved one. |
| `gateway.workspace` | `<state dir>/gateway/workspace` | Where the assistant's sessions work. |
| `gateway.notify_interval_secs` | `60` | One inbox message per source per interval. |
| `gateway.tools.allow` | all | Only these tools. |
| `gateway.tools.deny` | `[]` | Never these. |
| `gateway.tools.safe_mode` | `false` | Also deny `bash`, `write`, `edit`, `service`, `image_gen`. |
| `gateway.heartbeat.every_secs` | `0` (off) | A periodic turn on each listed chat. |
| `gateway.heartbeat.prompt` | a short "anything to say?" | What the heartbeat turn is asked. |
| `gateway.heartbeat.chats` | `[]` | Session keys to beat on, e.g. `deltachat:12`. |
| `gateway.scheduler_tick_secs` | `30` | How often due jobs and heartbeats are looked for. |
| `gateway.memory.enabled` | `true` | Core memory in the prompt, the archive behind the tools, a daily note at each compaction. |
| `gateway.review.enabled` | `true` | The review after a turn; see below. |
| `gateway.review.min_tool_calls` | `5` | An episode with fewer, and no error, is not reviewed. |
| `gateway.review.after_idle_secs` | just before the cache closes | Quiet time before the review runs. |
| `gateway.review.approval` | `false` | Stage the review's writes for `/approve`. |
| `gateway.weekly.enabled` | `true` | The weekly review of memory and skills; see below. |
| `gateway.weekly.cron` | `0 4 * * 1` | When it runs, UTC. |
| `gateway.weekly.stale_after_days` | `30` | A skill unused this long is named stale to the review. |
| `gateway.weekly.archive_after_days` | `90` | Unused this long, it is moved to `skills/.archive/`. |
| `gateway.status` | `true` | A status line in the chat while a turn runs. |
| `gateway.status_interval_secs` | `4` | The least time between two edits of it. |
| `gateway.announce` | `true` | A line to the last active chat when the gateway starts and stops. |
| `channels.deltachat.*` | — | The Delta Chat adapter; see below. |

### Delta Chat

The adapter spawns `deltachat-rpc-server` (`pip install
deltachat-rpc-server`, or any build on PATH) and speaks its JSON-RPC
over stdio; no bridge, no Python at run time.

| Key | Default | Meaning |
|---|---|---|
| `rpc_server` | `deltachat-rpc-server` on PATH | The server binary. |
| `accounts_dir` | `<state dir>/gateway/deltachat` | Where the account lives. |
| `setup_qr` | — | A `DCACCOUNT:` QR for a fresh chatmail identity (e.g. `DCACCOUNT:https://nine.testrun.org/new`). |
| `addr`, `password` | — | An existing address instead of the QR. |
| `display_name` | — | The name contacts see. |
| `allow_from` | `[]` | Addresses allowed to talk. A stranger gets no turn and no reply. |
| `allow_anyone` | `false` | Talk to whoever writes. Without it an empty `allow_from` refuses to start. |
| `ack_reaction` | — | An emoji to react with on receipt. |

A `.xdc` attachment — a zip with an `index.html` and a `manifest.toml`
— is sent as a webxdc app, which opens inside the chat; the model is
told so, and can build one with its ordinary tools.

The account is configured on first start and reused after. The
adapter ignores its own messages, info messages and other bots,
accepts a contact request from an allowed address, flags group chats,
and hands attachments to the turn as files. Replies go out as text, or
as a file message per attachment with the text on the first.

## How a message becomes a turn

`<channel>:<chat id>` is a session key. The first message on a key
creates a session; every later one resumes it, through
`<state dir>/gateway/routes.json`, which also records the last active
chat and which chats are groups. A chat's runtime stays open between
turns, so its background subagents keep running, and their completions
come back to the chat as follow-up turns exactly as they would reach a
TUI: as a prompt, retired from the outbox once the log holds it. A
session open in a TUI refuses the gateway's turn and the chat is told
so.

Turns on one chat are serialized; different chats run at once. Image
attachments are handed to the model the way `read` attaches them.

A message that arrives while the chat's turn is running does not wait
for it: it steers, as typing into the TUI mid-turn does. The loop
reads it at its next step boundary, the status line says "steered: …"
when the model has it, and one reply covers both messages; a message
arriving as the model stops reopens the turn rather than stranding
it. A slash command is still answered at once. Should the turn end
without reading it — it failed, or the gateway was stopping — the
message runs as a turn of its own afterwards.

## Watching a turn

While a turn runs for a chat, the bot posts "working…" and edits that
line as things move: "thinking — <topic>" from the reasoning summary,
"running bash: <command>", "delegating to explore: …", "writing…". The
line is deleted the moment the reply goes out, or when the turn ends
without one. On Delta Chat every edit and the deletion are messages
on the wire, so edits are spaced by `status_interval_secs`. Background
turns, cron and heartbeat, show nothing.

## Commands

A message that is a slash command is answered by the gateway itself:

| | |
|---|---|
| `/new` | A fresh session for this chat, on the configured model. The old one stays on disk; memory stays. |
| `/model` | The current model, then the models this configuration can reach by provider. |
| `/model <provider/model>` | Switch this chat. Recorded at once when the chat is idle, or as the running turn ends; the reply says which. The switch is the session's and outlives a restart; a session whose model is no longer configured cannot be resumed, and the chat starts over on the default. |
| `/model <provider/model> --save`, `/model --save` | Also make it, or the chat's current model, the default for new chats: kept as `<home>/model`, above `gateway.model`. |
| `/pending` | What the review staged, when approval is on. |
| `/approve [id\|all]`, `/reject [id\|all]` | Decide on it. |
| `/help` | The list above. |

## The home

Everything of the assistant's lives in one directory, `gateway.home`,
`~/.local/state/ilar/gateway/` unless set otherwise:

| | |
|---|---|
| `SOUL.md` | Who it is and how it talks. |
| `skills/`, `agents/`, `commands/` | Its own; the terminal agent's under `~/.config/ilar` are not read, nor the built-in skills, nor a working directory's `.ilar/skills`. A symlink shares one. |
| `memory/` | The core files, the notes, the daily notes. |
| `workspace/` | Where its sessions work. |
| `routes.json`, `cron.json`, `inbox/` | Chats, jobs, notifications. |
| `model` | The default for new chats, when `/model … --save` set one. |
| `deltachat/` | The channel's account, and `invite.txt`. |

Providers, keys and the `[gateway]` table itself stay in `ilar.toml`:
those are configuration; the home is the agent's own state, which it
will come to write itself.

## Where it is

After the base instructions every gateway session gets a short block
about its situation: that it is reached over a chat and answers
through the message tool, where its home and workspace are, that a
script can wake it with `ilar-gateway notify` and that it should use
that from cron jobs, services and long builds to report back, and that
scheduled turns speak only through the message tool. `ilar-gateway
prompt` shows it.

## Who it is: SOUL.md

A chat assistant needs a personality more than a coding agent does. A
gateway session reads `<home>/SOUL.md` where a terminal session reads
`~/.config/ilar/AGENTS.md`, and the workspace's own `SOUL.md` after
it, with `AGENTS.md` and `CLAUDE.md` as fallbacks in each of those two
places only. An assistant with no `SOUL.md` gets the base
instructions about tools and nothing else, so write one. Subagents
the assistant spawns are workers and read `AGENTS.md` from the home.

## Who may talk, and what the model may run

A channel names the senders it answers; anyone else is ignored
without a reply, since replying to a stranger is both a spam vector
and, on Delta Chat, an accepted contact request. A channel with no
allowlist refuses to start unless it is told `allow_anyone`.

The tool policy is enforced by construction, not by the prompt: a
denied tool is absent from the model's list, and the agents a chat
may spawn have their definitions narrowed before the spawner is
built, so a subagent cannot be the way around it. Safe mode is the
policy for a bot you do not want changing the machine.

## Memory that outlives a session

Two tiers, under `<state dir>/gateway/memory/`. The core is two small
files with hard caps, `MEMORY.md` (about the world, 2,200 characters)
and `USER.md` (about the person, 1,375), which the `memory` tool edits
with add, replace and remove; an overflow is an error the model
resolves by consolidating. The core is injected into the system
prompt once, when a chat's session opens, and stays frozen for that
session; it is never injected into a group chat.

The archive is one fact per file under `notes/`, typed as a decision,
solution, preference, event, task or risk, written with the same
tool's `note` action, plus daily notes under `daily/` that receive
every compaction handover. Nothing in the archive is ever injected:
`memory_search` returns an index, best first with recent notes
ranking higher, and `memory_get` reads the chosen notes in full.

## Skills it writes itself

`skill_manage` lets the assistant keep its own procedures under
`<home>/skills/`, in the `SKILL.md` layout the `skill` tool reads:
create, patch (the passage to replace must occur once, so a patch
changes only what it names), rewrite, delete. Two rules travel in the
tool's description, both Hermes's: lessons, not logs — a distilled
rule with its reason, never the story of what happened — and patch a
skill that exists before creating one. A new skill loads at once and
is listed in the prompt from the next session on. A ledger,
`skills/.usage.json`, counts views and patches per skill for the
weekly review. The review after a turn may create or patch skills
through the same library.

## The review after a turn

Once a chat has been quiet for a while after a turn — by default just
before the provider's prompt cache would go cold, so the conversation
is served from cache — the assistant is asked, as an aside that records
nothing, whether anything in the episode was worth keeping: a
preference or correction, a fact about its world, a decision, a
workflow that worked. Only an episode with enough tool calls, or an
error, is asked, and "nothing" is a welcome answer. Otherwise the
answer is a plan of memory entries and notes, written through the
memory store, and the chat gets one line: "💾 remembered: …". With
`gateway.review.approval` the plan is staged instead, the chat is told
what it would remember, and `/pending`, `/approve` and `/reject`
decide. Unlike Hermes, there is no bias toward action: most episodes
should end in nothing.

## The weekly review

A cron job the gateway owns, `weekly`, kept in step with the
configuration at every start. It runs on a background session of its
own, addressed to whichever chat was last heard from, with a fixed
prompt: read the week's daily notes, promote what recurs into the core
memory through the `memory` tool so the caps hold, drop what is no
longer true, file the rest as notes, merge overlapping skills through
`skill_manage`, and send one message saying what changed. Right before
it, a sweep that needs no model moves skills unused for ninety days to
`skills/.archive/` and names the ones unused for thirty, so the prompt
can ask about them.

## Scheduled turns: cron and heartbeat

The model has a `cron` tool: add a named prompt with a five-field cron
expression, an interval or a one-shot time, addressed to its own chat
or a known one; list; remove. Jobs live in `<state dir>/gateway/cron.json`.
A due job runs on its own session, `cron:<id>`, homed on the chat it
is for; a one-shot retires after firing. The heartbeat is the same
kind of turn on a fixed interval, on `heartbeat:<channel>:<chat>` for
each configured chat.

Neither kind of turn delivers its final text. A scheduled turn reaches
the chat only through the message tool, so a job or a heartbeat with
nothing to say says nothing.

## Replying: the message tool

Every gateway session has a `message` tool that knows its chat. It is
always present, whatever the tool policy says: a chat with no way to
answer is not a chat. The
model replies by calling it, so it can send several messages, attach
files, or say nothing; the turn's final text is delivered only when
the model sent nothing itself, and then exactly once. Another chat can
be named with `channel` and `chat`, but only one that has written to
the bot: the model does not open conversations with strangers. The
tool's description carries the channel's delivery constraints (for
Delta Chat: plain text, one message under 4000 characters, files by
absolute path).
