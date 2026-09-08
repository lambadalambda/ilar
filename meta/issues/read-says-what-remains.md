# read says what remains

## Summary

A windowed `read` that stops before the end of the file closes with a
bare `(truncated)`. Over 2026-09-05..07 that was 3,156 of 8,219 read
results: the model reads in windows of 100–300 lines and, at every
window's end, has to guess the next offset and whether the file goes on
much further.

## Requirements

- The closing marker names the window shown, the file's total line
  count, and the offset to continue from — both when the line limit
  ends the window and when the byte cap does.
- A window that reaches the end of the file carries no marker, as now.
- Counting the rest of the file stays streaming and cancellable.

## Acceptance Criteria

- Tests for the limit-ended and cap-ended windows assert the marker's
  numbers; the existing read tests still pass.
