# Export transcript to Markdown

## Summary

No way to share or archive a session outside the JSONL.

## Requirements

- Palette entry "Export transcript" writes Markdown to the cwd
  (ilar-transcript-<id-prefix>.md): user/assistant text, thoughts as
  collapsed quotes, tool calls one-line with results fenced, system
  notes.
- Notice shows the written path; failures surface as error notices.

## Acceptance Criteria

- Unit test rendering a mixed transcript to Markdown.
