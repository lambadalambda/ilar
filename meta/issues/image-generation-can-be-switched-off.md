# Image generation can be switched off

## Summary

The `image_gen` tool appears whenever the openai provider has a
credential. A ChatGPT login used for text should not have to bring
image generation along: it costs, and a bot's model may be talked
into it.

## Requirements

- `providers.openai.image_gen = false` leaves the tool out of the
  registry; absent or `true` keeps today's behaviour.
- The key on any other provider is a configuration error, since the
  tool rides openai's credentials only.

## Acceptance Criteria

- Tests: the backend resolves with the key absent, not with it
  false; the key on another provider is refused at load.
- A configuration row.
