# The gateway drives a session per chat

## Summary

A long-lived process that owns channels and routing and runs ilar
turns. First cut drives `ilar exec --continue --json` per inbound
message so the shape can be tried in a day; the library's
`runtime::SessionRuntime` replaces the subprocess once the shape holds.

## Requirements

- New crate `ilar-gateway` (binary), config under the existing
  `ilar.toml` (`[gateway]`, `[channels.<name>]`).
- One writer per session: a turn runs only if the gateway can take the
  session's writer lease; a session open in a TUI is watch-only.
- Inbound and outbound go through one in-process bus with the shapes
  picoclaw uses (channel, chat id, sender id, content, media).
- A local inbox: `ilar notify "…"` from scripts lands as an inbound
  message to the last active chat, rate-limited per source.

## Acceptance Criteria

- A fake channel round-trips a message to a turn and back; the lease
  refusal is tested; the inbox delivers.
