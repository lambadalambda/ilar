# Chats route to sessions; the message tool

## Summary

`channel:chat_id` names a session. The gateway keeps the mapping, the
"last active chat" default target, and a private/group flag per chat.
The model sends through a `message` tool rather than by returning
text, so it can send several, send to another chat, or send nothing.

## Requirements

- Session keys as picoclaw: `<channel>:<chat>`, `heartbeat:…`,
  `cron:…`; background keys never target a chat unless scoped.
- `message` tool: channel, chat (default: the turn's own), text, media;
  the adapter's constraints in its description.
- A turn's final text is delivered only if the model sent nothing.
- The group flag gates what memory is injected (see memory).

## Acceptance Criteria

- Two chats keep two sessions; a cron turn cannot message an
  unscoped chat; the fallback delivery happens exactly once.

## Status (2026-09-08)

Done: session keys and the routes file (with the gateway issue), the
`message` tool with its constraints and the known-chat restriction,
the final-text fallback exactly once, the group flag recorded. Left
for the cron issue: the background keys and the rule that an unscoped
background turn may not message a chat.
