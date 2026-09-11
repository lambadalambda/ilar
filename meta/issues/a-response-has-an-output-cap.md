# A response has an output cap

## Summary

No `max_tokens` is sent, so a looping model generates until the
context fills: an hour of GPU for one response. A cap per response,
generous and configurable, ends that in minutes and says why.

## Requirements

- `agent.max_output_tokens`, default 32768, sent as the dialect's
  output cap on every request the loop makes; `0` sends none.
- A response stopped at the cap is reported in the transcript and to
  a chat: stopped at the cap, and how to raise it.

## Acceptance Criteria

- Tests: the cap reaches both wire forms; a max-tokens stop reads as
  such in the turn's outcome.
