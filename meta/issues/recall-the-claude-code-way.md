# Recall the Claude Code way

## Summary

ilar's selector sees one message alone, and a recall carries index
lines only. Claude Code's selector is one conversation per session —
the memory listing, then each earlier user message with the answer it
got — and a recall carries each memory's text. A follow-up like "are
you sure this is right?" means nothing alone and something after the
messages before it. (The selector sees the person's messages and its
own answers, not what the agent did.)

## Requirements

- History (default on): the selector request is the listing, then every
  earlier typed message of the window with the picks it got (rebuilt
  from the log: user messages and `MemoryRecall` events), then the new
  message. The listing holds every note, oldest first, so it stays the
  cached prefix and earlier numbers stay valid; held notes are dropped
  when the answer arrives. The prompt says not to pick a note again.
- Bodies (default on): the recall block carries each picked note's
  body, at most 4 KB each; the window's recall budget is 60 KB.
- `general.memory_recall_history = false` and
  `general.memory_recall_bodies = false` restore today's behavior.

## Acceptance Criteria

- Tests for both modes: request shape, history rebuilt from the log,
  bodies in or out of the block, config.
- A live probe through the ChatGPT login: the second selector call in a
  session is served mostly from cache.
