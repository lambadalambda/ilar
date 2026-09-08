# Cron and heartbeat turns

## Summary

Both reduce to "start a turn on session X with prompt Y at time T and
deliver what it sends", which is the notification-driven turn ilar
already runs for child completions.

## Requirements

- A cron store (JSON under the state dir): schedule (cron expr, interval
  or one-shot), prompt, target chat, last/next run; a `cron` tool to
  add, list and remove.
- Heartbeat: a periodic turn on `heartbeat:<channel>:<chat>` with a
  configurable prompt; silent unless the model uses the message tool.
- Delivery only through the message tool (picoclaw's rule).

## Acceptance Criteria

- A one-shot job fires once and is retired; a heartbeat with nothing to
  say sends nothing.

## Status (2026-09-08)

Done: `cron.json` store, `cron` tool (add / list / remove; cron
expression, interval or one-shot), heartbeat per configured chat,
both on their own sessions with delivery only through the message
tool; a one-shot retires. The unscoped case never arises: every job
and beat is homed on a chat that has written.
