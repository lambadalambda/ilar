# The prompt preview shows the whole request

## Summary

`ilar --print-prompt` and `ilar-gateway prompt` print the system
prompt alone. What the model gets is more: the tool list with every
description and schema, the request options a reasoning variant
adds, the model itself. Reading the prompt to judge what the agent
sees means reading all of it.

## Requirements

- Both commands print the model, the reasoning variant and request
  options, the agent, the system prompt as sent, and every tool the
  session would have with its description and input schema.
- Built through the same code a real session uses for its tools, so
  the preview cannot drift; for the gateway, with the chat's own
  tools (message, memory, cron, skill_manage) under the tool policy.
- No session is created by a preview.

## Acceptance Criteria

- Tests: a preview of a test configuration renders the `read` tool
  and leaves the session store empty; a gateway preview renders the
  message tool for the channel and the situation block.
