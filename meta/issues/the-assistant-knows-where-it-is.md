# The assistant knows where it is

## Summary

A gateway session's prompt says nothing about its situation: not that
it is reached over a chat, where its home is, that scripts can wake
it with `ilar-gateway notify`, or that scheduled turns speak only
through the message tool. Hermes and OpenClaw both carry a short
"your situation" block; ours would be a few lines.

## Requirements

- A block appended to every gateway session's system prompt, after
  the base instructions: reached over a chat channel and answered
  through the message tool; the home and workspace paths; the notify
  command and when to use it; scheduled turns and heartbeats speak
  only through the message tool.
- Shown by `ilar-gateway prompt`.

## Acceptance Criteria

- Test: the prompt of a chat contains the block with the home path.
