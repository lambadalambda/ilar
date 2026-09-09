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

## Notes

- Done 2026-09-09. Found on the way: systemd's default `KillMode`
  signalled the rpc server too, so the stop line had nothing to go
  through, and the shutdown always waited the full dispatcher grace
  because the seats keep senders to the queue. The unit now has
  `KillMode=mixed` and `TimeoutStopSec=25`; the dispatcher drains and
  ends on a token raised after the driver stops. Re-copy the unit file
  on an existing install.
