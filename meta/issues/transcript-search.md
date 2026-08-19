# Transcript search

## Summary

Long sessions have no way to find text in scrollback.

## Requirements

- Ctrl-F opens a search prompt in the status area; typing filters live.
- Up/Down jump between matches (scroll follows); Enter closes keeping
  position; Esc closes and restores the pre-search scroll.
- Matching rows are visibly highlighted while search is active; a match
  counter (n/m) is shown.
- Query persists for the session; Ctrl-F reopens with the last query.

## Acceptance Criteria

- Tests: match collection over rendered rows, jump ordering, Esc restore.
