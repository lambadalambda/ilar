# Image generation on the OpenAI account

## Summary

Codex can generate and edit images on a ChatGPT subscription; ilar
cannot. Codex's implementation is not a Responses built-in: an
`image_gen` function tool posts JSON to `{provider base}/images/
generations` (or `/images/edits` with `images: [{image_url: data-url}]`)
carrying the same bearer, `chatgpt-account-id` and `originator`
headers as the Responses wire, model `gpt-image-2`, and decodes
`data[0].b64_json` into a PNG it saves and shows. Requested by the user
2026-09-05.

## Requirements

- An `image_gen` tool installed when the openai provider is configured:
  ChatGPT login → `https://chatgpt.com/backend-api/codex/images/…`;
  API key → `https://api.openai.com/v1/images/…`. Same 401
  refresh-and-retry as the Responses provider.
- Arguments: `prompt` (required), `size` (`auto` default or
  `WIDTHxHEIGHT`), `quality` (`auto|low|medium|high`), and
  `reference_paths` (up to 5 image files) which switch to the edits
  endpoint. Model `gpt-image-2`.
- The PNG is written under `<state dir>/images/<session>/<call>.png`
  and the result names the path; the image rides the tool result as an
  attachment so a vision model sees what it made and the transcript
  shows the marker.
- Mutating/barrier tool (it spends money and writes a file); a
  timeout generous enough for 4K output.
- docs: configuration.md (when it appears) and agents-and-skills.md.

## Acceptance Criteria

- Wire tests against a local server: generation posts the right path,
  headers and body for each auth mode; an edit carries the reference
  as a data URL; the response is saved and attached; a 401 refreshes
  once.
- A live generation through the user's login produces a file.

## Outcome (2026-09-05)

`image_gen` (crates/ilar/src/tools/image_gen.rs), installed by the
runtime when `[providers.openai]` has a ChatGPT login or an API key —
the login posts to `https://chatgpt.com/backend-api/codex/images/…`
with bearer, `chatgpt-account-id` and `originator`, the key to
`https://api.openai.com/v1/images/…` with a bare bearer; one refresh
on 401. Arguments `prompt`, `size`, `quality`, `reference_paths` (up to
five, sent as data URLs to `/images/edits`). Model `gpt-image-2`. The
PNG lands at `<state dir>/images/<session>/<call>.png`; the result names
the path and attaches the (bounded) image. Wire tests cover both auth
modes, the edit shape, the saved file and attachment, and argument
refusals. Live: one low-quality 1024² generation through the user's
login answered in 20 s with a 743 KB PNG of exactly the prompt. Not
tested live: the 401 refresh path and API-key edits (the public API's
edits endpoint is multipart, which this JSON body does not speak —
edits are a ChatGPT-backend feature for now, noted in the docs to
add).
