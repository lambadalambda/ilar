# Provider state goes back only to its model

## Summary

`transcript_of` ignores which model wrote an assistant message, and the
Responses request builder sends every stored reasoning item back. After
a mid-session switch (Muse Spark to GPT, say), the new model receives
the old one's encrypted reasoning, phase marks and item ids. At best
it is ignored; OpenCode users have seen "encrypted_content was not
issued to this caller" errors from it. The side requests (aside, topic,
compaction summary) send the session's transcript to other models too.

OpenCode replays provider metadata only when the message's model is
the request's model (`to-llm-message.ts`, `reuseProviderMetadata`).

## Requirements

- A transcript built for a model strips, from assistant messages another
  model wrote: `Reasoning` items, `Text.phase`, `ToolCall.item_id`.
  Text, tool calls, results and readable `Thinking` stay.
- The main turn and every side request build their transcript for the
  model they send to.

## Acceptance Criteria

- A store test: a session with messages from model A and B, built for
  A, keeps A's state and strips B's; built for B, the reverse.
- Messages from the request's own model are byte-identical to today, so
  the cached prefix of a session that never switches does not move.
