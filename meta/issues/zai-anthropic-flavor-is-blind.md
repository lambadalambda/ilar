# The zai Anthropic flavor is blind on 4.6v/4.5v

## Summary

Live probes (2026-08-25) show the z.ai Anthropic-compatible gateway
silently discards every image block for `glm-4.6v` and `glm-4.5v` —
including images in ordinary user messages, i.e. the shipped Ctrl-V
feature. Conclusive: invalid base64 returns HTTP 200 for those two
(nothing decodes it) but HTTP 400 for `glm-5v-turbo`, which sees
correctly on the same route. Under the OpenAI-compatible flavor all
three V-models see. `Flavor::default()` is Anthropic and the default
config sets no flavor — so the default configuration confabulates
plausible image descriptions on two of the three vision models,
failing in the worst possible way.

## Requirements

- Preferred: route zai requests that carry images through the
  OpenAI-compatible flavor regardless of configured flavor (the
  models are identical; only the gateway differs), with a comment
  citing the probe evidence.
- Alternatively (or additionally): the catalog/models listing marks
  the pair, and attaching an image on the broken combination warns
  instead of silently degrading.
- Re-probe before assuming the gateway got fixed.

## Acceptance Criteria

- A wire test: an image-bearing request for `zai/glm-4.6v` under
  the default (Anthropic) flavor goes out in the OpenAI-compatible
  shape (or the chosen alternative behavior, once decided).

## Notes

- Discovered during the tool-results-can-carry-images design pass.
- The user's own config sets `flavor = "openai"`, which is why
  their live Ctrl-V test on 4.6v worked.

## Milestone

12 — Health sweep
