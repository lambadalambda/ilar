# Thinking goes back whole, and under its own name

## Summary

Two differences from OpenCode's replay, both decided against ilar's
first cut after reading their `transform.ts`:

1. **Scope.** ilar sends `reasoning_content` on the assistant messages
   of the current turn only. OpenCode sends it on every assistant
   message in the conversation, and for DeepSeek fills in an empty one
   where none exists. If the models are trained on the whole history
   of their thinking, the current-turn rule starves them. The default
   becomes "all", with a switch for "turn" (and "off").
2. **Spelling.** The mapper reads thinking under both `reasoning_content`
   and the OpenRouter-style `reasoning`, but echoes everything back as
   `reasoning_content`. A model that streamed `reasoning` should get it
   back as `reasoning`. The live probe on `opencode/kimi-k3` showed the
   gateway *accepts* the other spelling; whether the upstream uses it
   nobody can tell from here.

## Requirements

- `replay_thinking = "all" | "turn" | "off"`: `all` is the default;
  `[general]` sets it for every chat-wire model; a `[models.*]` or
  `[endpoints.*]` entry overrides it for that server.
- A persisted `Thinking` block remembers which field it arrived under
  when that was not `reasoning_content`; the wire echoes it under the
  same one.
- Earlier turns' thinking goes back too under `all`; the two-turn
  probe still passes against every family it covers.

## Acceptance Criteria

- Wire tests: `all` sends thinking on both turns' assistant messages,
  `turn` on the current turn's only, `off` on none; a block that
  arrived as `reasoning` goes back as `reasoning`.
- A mapper test: a `reasoning` delta stamps the spelling once per
  response; a `reasoning_content` delta stamps nothing.
- docs/configuration.md documents the switch and the spelling rule.

## Notes

Follow-up to [interleaved-thinking-goes-back-on-the-wire] (archived),
after comparing with OpenCode 2026-09-18. `reasoning_details`
(OpenRouter's array form) is not read on the way in and so not echoed.

Size: M. Source: comparison with OpenCode, 2026-09-18.
