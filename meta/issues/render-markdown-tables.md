# Render Markdown tables

## Summary

Markdown tables currently appear as raw pipe-delimited source in the TUI,
making structured answers difficult to scan.

## Requirements

- Parse standard GitHub-style Markdown tables.
- Render clear headers, column boundaries, and row alignment.
- Preserve inline Markdown styling inside cells.
- Keep table rows within the available transcript width.
- Degrade readably on narrow terminals and while content is still streaming.

## Acceptance Criteria

- Complete tables render without raw delimiter rows.
- Column alignment markers are respected where practical.
- Long and Unicode cell content cannot overflow the viewport.
- Incomplete streaming table syntax remains visible rather than disappearing.
- Existing Markdown rendering tests and full workspace checks pass.
