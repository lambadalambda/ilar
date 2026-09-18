# Interleaved thinking goes back on the wire

## Summary

ilar never sends a model's thinking back. Every thinking block is
persisted as a local diagnostic (`agent/turn.rs`, `content_blocks`)
and the chat-completions wire drops it when it rebuilds the
conversation (`provider/chat.rs`, `openai_message`). For models whose
thinking is *interleaved* with tool calls — Qwen3, GLM, Kimi K2,
MiniMax — that is wrong: their chat templates keep `reasoning_content`
on the assistant messages after the last user message, so the model
carries its own plan through a tool loop, and the vendors say to pass
it back within a turn.

Seen in a charachat session on 2026-09-18 (`8fbb6ec7`, Qwen3.8 flash
both local and via halogen): at every step inside a turn the model
starts from an empty think block and fills the gap — eleven thinking
blocks claim a previous reply of "Understood. I will follow these
instructions." that was never given, one describes a working directory
from its training data. The actions that followed were right, so the
cost is tokens and a re-plan per step. Three turns also ended on
"Writing the card tests:" with no tool call, the plan having lived only
in thinking the model never got back; that half is plausible, not
proven.

## Requirements

- On the chat-completions wire, an assistant message after the last
  user message carries its thinking as `reasoning_content`, for models
  that think this way. Earlier turns' thinking is not sent: every
  vendor agrees it is dropped there, and one (DeepSeek) refuses it.
- Which models: the catalog's interleaved flag where there is a
  catalog row; a discovered endpoint model whenever it streamed
  reasoning at all — if it gave `reasoning_content`, its template
  takes it.
- Thinking that will be replayed is persisted as thinking, not as a
  local diagnostic: the log says what the wire does with it. Models
  whose thinking is never replayed keep the diagnostic.
- Every reader of a persisted `Thinking` block — the TUI fold, the web
  view, recall — shows it the way it shows a diagnostic today.

## Acceptance Criteria

- A wire test: a conversation `user, assistant(thinking, tool_call),
  tool, assistant(thinking, text)` serialises with `reasoning_content`
  on both assistant messages when the model is interleaved, and on
  neither when it is not; a second user message ahead of them strips
  the earlier turn's.
- The turn loop persists `Thinking` for an interleaved model and
  `Diagnostic { Local }` otherwise.
- docs/providers (wherever thinking on the wire is described) say
  which models get their thinking back and why.

## Notes

The OpenAI responses wire has its own reasoning items and is not
touched; Anthropic-wire is not built yet.

Size: M. Source: charachat session forensics, 2026-09-18.
