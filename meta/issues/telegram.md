# Telegram

## Summary

The first real channel. picoclaw's adapter is ~700 lines of Go and a
useful checklist: long polling, media download, markdown flavour,
message length limit, typing indicator, reply threading.

## Requirements

- Bot API over reqwest, no framework; long polling.
- Delivery constraints (formatting, 4096-char splits) live in the
  adapter, and are described to the model by the message tool.
- Media in: images attach to the turn as `read` attaches them.

## Acceptance Criteria

- A message to the bot from an allowlisted sender gets an answer;
  images are seen; long answers arrive whole.
