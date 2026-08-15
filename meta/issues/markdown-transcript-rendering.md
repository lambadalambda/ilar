# Markdown transcript rendering

## Summary

Assistant responses are displayed as raw Markdown inside a single logical terminal line, making headings, lists, code, links, and paragraphs difficult to read.

## Requirements

- Render assistant Markdown as structured Ratatui lines.
- Visually distinguish headings, emphasis, inline code, fenced code, lists, quotes, and links.
- Preserve readable partial output while the response is streaming.
- Keep user, tool, and system transcript entries visually distinct.

## Acceptance Criteria

- Renderer tests cover headings, lists, emphasis, inline code, fenced code, links, quotes, and multiline paragraphs.
- Rendered spans contain no embedded newline characters.
- Markdown output uses terminal-safe styling and preserves code whitespace.
- Workspace tests, formatting, and clippy pass.
