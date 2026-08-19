# Fuzzy session search in the picker

## Summary

The session picker is a plain list; finding an older session means
scrolling.

## Requirements

- fzf-style fuzzy filter: typing narrows the list (subsequence match,
  scored: consecutive runs and word starts rank higher); best match
  selected.
- Case-insensitive; matches against title and id.
- Backspace edits the query; the query shows in the picker chrome.

## Acceptance Criteria

- Unit tests for the matcher (ordering, boundaries, unicode) and the
  picker filtering.
