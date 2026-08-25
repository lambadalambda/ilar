# GLM vision models are hidden on the coding plan and get no images

## Summary

The V-series (`glm-5v-turbo`, `glm-4.6v`, `glm-4.5v`) is tiered
API-only in the catalog, so a coding-plan config (`flavor = "openai"`)
hides them from the model picker — but all three answer on the
coding-plan endpoint (verified live with a real key; models.dev's
international record is stale). And even when selected, the zai wire
degrades every image to an `[image omitted]` placeholder, so a GLM
vision model would never actually see one.

## Requirements

- The V-series is available with a zai API key under either flavor.
- The zai wire sends real image parts to vision models — base64
  `image` source blocks on the Anthropic flavor, `image_url` data-URL
  parts on the OpenAI flavor — and keeps the named placeholder for
  models without vision.
- Text-only messages keep their exact current wire shape on both
  flavors.

## Acceptance Criteria

- With a coding-plan config, the picker lists the V models.
- Wire tests: image parts on a vision model, placeholder on a
  non-vision model, both flavors; text-only shape unchanged.
- Manually: paste an image into a GLM-V session and get an answer
  that proves the model saw it.

## Milestone

11 — Beyond the terminal

## Outcome

The V-series moved to `ZaiBoth` — grounded not in models.dev (whose
international coding-plan record omits them) but in a live probe: all
three answered on the coding-plan endpoint with the real key. The zai
wire now threads `supports_vision` per request: vision models get real
parts (`image_url` data URLs on the OpenAI flavor, base64 `image`
source blocks on the Anthropic flavor), text models keep the named
`[image omitted]` gap, and text-only messages keep their exact old
shapes on both flavors.

Verified live end to end on the coding plan: the picker lists all
three V models, and GLM-4.6V answered a pasted green/yellow "GLM 7"
test image with "green and yellow, GLM 7".
