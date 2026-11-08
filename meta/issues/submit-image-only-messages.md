# Submit image-only messages

## Summary

Enter ignores an attached image when the prompt has no text.

## Requirements

- Submit attached images without requiring text, both idle and during a running turn.
- Preserve normal steering and queue behavior, including queue delivery.
- Keep an empty prompt with no attachments inert.

## Acceptance Criteria

- Tests cover image-only submission as a fresh turn, a steer and a queued message.
- Queued image-only messages reach the next turn with their images intact.
- Empty submissions without images do not start a turn.
- Relevant tests pass.
