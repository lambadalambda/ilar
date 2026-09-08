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
```

## Configuration

Both tables live in the user's `ilar.toml` only; a project file that
declares them is warned about and ignored.

| Key | Default | Meaning |
|---|---|---|
| `gateway.agent` | the core's default | The agent every chat runs as. |
| `gateway.model` | `general.model` | `provider/model` for every chat. |
| `gateway.workspace` | `<state dir>/gateway/workspace` | Where the assistant's sessions work. |
| `gateway.notify_interval_secs` | `60` | One inbox message per source per interval. |
| `gateway.tools.allow` | all | Only these tools. |
| `gateway.tools.deny` | `[]` | Never these. |
| `gateway.tools.safe_mode` | `false` | Also deny `bash`, `write`, `edit`, `service`, `image_gen`. |
| `gateway.heartbeat.every_secs` | `0` (off) | A periodic turn on each listed chat. |
| `gateway.heartbeat.prompt` | a short "anything to say?" | What the heartbeat turn is asked. |
| `gateway.heartbeat.chats` | `[]` | Session keys to beat on, e.g. `deltachat:12`. |
| `gateway.scheduler_tick_secs` | `30` | How often due jobs and heartbeats are looked for. |
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

Every gateway session has a `message` tool that knows its chat. The
model replies by calling it, so it can send several messages, attach
files, or say nothing; the turn's final text is delivered only when
the model sent nothing itself, and then exactly once. Another chat can
be named with `channel` and `chat`, but only one that has written to
the bot: the model does not open conversations with strangers. The
tool's description carries the channel's delivery constraints (for
Delta Chat: plain text, one message under 4000 characters, files by
absolute path).
