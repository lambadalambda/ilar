# TUI cursor and markdown density

## Summary

Text entry lacks a visible terminal cursor, and rendered Markdown uses excess
vertical spacing and loses paragraph indentation on wrapped lines.

## Requirements

- Show the native blinking cursor while editing the prompt or model search.
- Omit blank Markdown separator rows from transcript presentation.
- Keep every visual line of an assistant paragraph aligned after its label.
- Preserve Markdown styling and code-block contents.

## Acceptance Criteria

- Idle prompt and picker search rendering set visible cursor positions.
- Markdown paragraph separators do not produce blank transcript rows.
- Wrapped assistant lines retain the same five-column content indentation.
- Focused rendering tests and all workspace checks pass.
