# Resume failed turns and retry transient provider errors

## Summary

Retry a failed provider request from the current accumulated conversation instead of resubmitting the original user prompt, and automatically recover from transient provider failures with bounded exponential backoff.

## Requirements

- Preserve the completed assistant/tool-call chain when a provider request fails.
- Make the Ctrl-R retry affordance continue from the current conversation state without appending the original prompt again.
- Do not rerun already completed tools when retrying the failed provider request.
- Automatically retry transient provider/transport failures with bounded backoff.
- Do not automatically retry permanent protocol, authentication, or request errors.
- Keep retries cancellable and expose retry activity to the UI.

## Acceptance Criteria

- A test covering a failure after one or more completed tool rounds proves that manual retry sends the accumulated chain and does not duplicate the original prompt or completed tool calls.
- Tests prove transient failures retry with bounded backoff and eventually succeed or return the final error after the retry limit.
- Tests prove non-transient failures are returned without automatic retry.
- Workspace formatting, tests, and clippy pass.

## Notes

- Automatic retries should apply to the individual failed provider call, not restart the whole user turn.
