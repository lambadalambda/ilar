# Present OpenAI reasoning summaries

## Summary

OpenAI Responses models stream short reasoning-summary text that can explain the
current activity, but ilar does not currently present it as a concise thought
status in the transcript.

## Requirements

- Identify and map the provider's actual reasoning-summary events.
- Keep private chain-of-thought separate from provider-supplied summaries.
- Stream concise thought summaries through the neutral event model and TUI.
- Preserve useful summaries across session resume without duplicating content.

## Acceptance Criteria

- OpenAI reasoning-summary fixtures produce the expected neutral events.
- The TUI renders streamed summaries as distinct thought status rows.
- Completed summaries survive persistence and resume.
- Existing OpenAI and z.ai reasoning behavior remains unchanged.
- Full workspace checks pass.
