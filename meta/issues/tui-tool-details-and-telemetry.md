# TUI tool details and telemetry

## Summary

The transcript hides tool arguments, the status area omits persistent runtime
context, and model activity is visible only below the transcript.

## Requirements

- Render tool arguments after the state icon and tool name in a muted style.
- Keep each tool row to one terminal line and truncate it to the viewport.
- Always show current model, working directory, token usage versus context
  limit, percentage used, and ready/thinking/responding state above input.
- Truncate status fields sensibly on narrow terminals while preserving the
  highest-value information.
- Show animated assistant activity in the transcript while a response is in
  progress.
- Preserve scrolling, streaming, and completed transcript behavior.

## Acceptance Criteria

- Tool rows expose compact arguments without wrapping.
- The status strip remains present while idle and busy.
- Token counts and percentages update from actual usage/context data.
- Narrow layouts do not panic or push the input area off-screen.
- Thinking/responding activity is visible in the main transcript and clears
  when the turn ends.
