# The gateway announces itself

## Summary

After a deploy or a restart nothing in the chat says the gateway
changed; the only trace is the journal. One line on start and one
on stop, to the last active chat, makes a restart visible where the
person is looking.

## Requirements

- On start, once the channel can send: `▶ ilar-gateway <version>
  (<commit>) started · model <provider/model>`, to the last active
  chat. Nothing when no chat has written yet.
- On stop (Ctrl-C or `systemctl stop`): `⏹ ilar-gateway stopping`,
  to the same chat, before the channels go down.
- `systemctl stop` sends SIGTERM; the gateway handles it like Ctrl-C.
- Off with `gateway.announce = false`.

## Acceptance Criteria

- Tests: a stop line on cancel, a start line from a fresh gateway on
  the same home, nothing with `announce = false`.
- The start line on tenco after a restart shows the deployed commit.
