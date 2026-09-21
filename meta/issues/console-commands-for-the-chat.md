# Console commands for the chat

## Summary

The gateway has fourteen commands and sits on state it never shows in
a chat: what the model is, whether a turn is running, what the session
has cost, what jobs are scheduled, what subagents are doing. Each is a
read of something that already exists; together they are the
difference between a chat and a console. From
[[gateway-comparison-2026-09-21]], §2.

## Requirements

- `/status` — the chat's model, whether a turn is running and for how
  long, subagents running, asks waiting, and the session's context
  size after the last turn.
- `/cost` — this session's spend: tokens in and out, cache reads, and
  the price where the model is priced; "not priced" where it is not.
  Over the whole log, not the window since the last compaction.
- `/cron` — the jobs, with schedule, next run and target; `/cron remove
  <id|name>` takes one away. The model's `cron` tool keeps `add`; a
  person adds by asking.
- `/tasks` — subagents running for this chat, with agent, description
  and elapsed time, and results held for delivery.
- `/whoami` — the sender id and chat id as the channel reports them,
  which is what `allow_from` wants; the one command a stranger may
  never see, since a stranger is never answered.
- `/restart` — announce, drain as a stop does, and exit with a code
  the service unit restarts on.
- Every new command is in `commands::HELP` and, through
  [[a-telegram-channel]], in the Telegram menu.

## Acceptance Criteria

- An integration test per command against a `FakeChannel`, asserting
  the reply's content: a priced model's `/cost` shows a dollar figure
  after a turn with usage; `/cron remove` empties the store; `/status`
  says "running" during a turn and "idle" after.
- `/restart` exits the process with the documented code; the systemd
  unit is updated to restart on it.
- `docs/gateway.md`'s command list is updated.

## Notes

- Left out on purpose, each its own issue if wanted: `/sessions`
  (cross-session search and switching is a bigger piece), `/rewind`
  and `/fork` (checkpoints are taken for gateway turns but nothing
  restores one from a chat).
- Size: S–M. Source: user request, 2026-09-21.
