# Refresh OpenCode model catalog and prices

## Summary

Add the missing Zen and Go models identified in the catalog audit and refresh OpenCode prices from the live listings.

## Requirements

- Add Go: kimi-k3.1-pro, minimax-m3.1-flash, hy4, crouton-alpha-free, iceman-contributor-free.
- Add Zen: muse-wonder-1.0, mistral-large-4, qwen3.8-max, crouton-alpha-free, iceman-free, inkling-2-small-free, nemotron-3.5-super-vl-free.
- Map full model metadata: wire, reasoning levels, reasoning field, modalities, limits, costs, and deprecation status.
- Refresh prices of existing OpenCode entries, including Grok 4.7.
- Support Mistral's native typed text/thinking stream and tool-result replay.
- Keep price estimates at published base rates, consistent with the existing flat-price API; document context-tier limits.
- Keep the deferred fork snapshot issue untouched.

## Acceptance criteria

- Catalog tests cover new rows, exact reasoning ladders, vision/input/output limits, and prices.
- Relevant provider and model tests pass, with independent review before commit.

## Sources

- https://opencode.ai/inference/v1/models
- https://opencode.ai/inference/go/v1/models
- https://models.dev/api.json
