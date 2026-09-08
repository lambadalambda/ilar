# The assistant — index

## Summary

ilar as the core of a hermes/openclaw/picoclaw-style always-on
assistant, on DeltaChat first, without Claude support (decided
2026-09-08). The loop, sessions, subagents, tools and headless `exec`
are the hard part and exist; what is missing is the shell around them.
The picoclaw fork at ~/repos/picoclaw is the reference: its assistant
layer (channels, bus, routing, cron, heartbeat, inbox, message tool,
policy) is ~5k lines of Go around a 1.5k-line loop.

## Order

1. [The gateway drives a session per chat](the-gateway-drives-a-session-per-chat.md)
2. [DeltaChat](deltachat.md)
3. [Chats route to sessions; the message tool](chats-route-to-sessions.md)
4. [A sender allowlist and a tool policy](a-sender-allowlist-and-a-tool-policy.md)
5. [Cron and heartbeat turns](cron-and-heartbeat-turns.md)
6. [Memory that outlives a session](memory-that-outlives-a-session.md)

## Rules

- A separate crate. The core and the TUI stay untouched except where
  an issue names the change (memory; possibly a provider).
- The gateway stays thin: a turn's final text and the loop's events,
  never a transcript view. `ilar serve` was stood down for growing one.
- Nothing is exposed to a channel before 4 is done.

## Status (2026-09-08)

Steps 1, 3, 4, 5 and 6 done and live on tenco; step 2 (DeltaChat) is
live and answering, with its remaining acceptance items (an image in,
a file out, a stranger ignored, the rpc server dying) still owed. The
memory issue's later items are listed on it.
