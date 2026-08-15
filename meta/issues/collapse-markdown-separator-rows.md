# Collapse Markdown separator rows

## Summary

Removing every Markdown blank row made distinct blocks too dense; previously,
runs of blank lines could create excessive vertical space.

## Requirements

- Render exactly one empty row for an interior Markdown blank-line run.
- Ignore leading and trailing blank-line runs.
- Preserve blank lines inside fenced code exactly as code rows.

## Acceptance Criteria

- Paragraphs separated by one or more blank source lines have one empty row.
- Final Ratatui rendering preserves that separator as exactly one terminal row.
- Leading and trailing blank lines add no transcript rows.
- Focused Markdown and workspace tests pass.
