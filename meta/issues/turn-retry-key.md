# One-key turn retry

## Summary

A failed turn consumes the prompt; retrying requires history recall.

## Requirements

- On turn error, keep the last user prompt and show "press r to retry"
  in the failure notice.
- `r` on a blank input while idle resubmits the prompt verbatim.
- Any manual submit or session switch clears the retry state.

## Acceptance Criteria

- Tests: retry state set on error, cleared on submit; key gating.
