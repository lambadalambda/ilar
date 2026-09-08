# ilar-gateway

ilar as an always-on assistant: a process that listens on messaging
channels, runs each chat as its own ilar session on the library
runtime, and sends the answer back. Milestone 21 in `meta/issues.md`;
this page grows with it.

```sh
ilar-gateway                       # listen on the configured channels
ilar-gateway notify "build green"  # a message from a script, to the last active chat
ilar-gateway notify --to deltachat:12 --source ci "…"
```

## Configuration

Both tables live in the user's `ilar.toml` only; a project file that
declares them is warned about and ignored.

| Key | Default | Meaning |
|---|---|---|
| `gateway.agent` | the core's default | The agent every chat runs as. |
| `gateway.workspace` | `<state dir>/gateway/workspace` | Where the assistant's sessions work. |
| `gateway.notify_interval_secs` | `60` | One inbox message per source per interval. |
| `channels.<name>.*` | — | A channel adapter's own settings. None are wired yet. |

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

Turns on one chat are serialized; different chats run at once. The
final text of a turn is the reply; a turn that says nothing sends
nothing. Image attachments are handed to the model the way `read`
attaches them.
