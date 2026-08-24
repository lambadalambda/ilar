# The session search opens like a picker

## Summary

The content grep is the front door for switching sessions, but opening
it shows an empty pane and a hint. Three gaps, one surface:

- **Empty query shows nothing.** To hop to a recent session you must
  remember something said in it, or know about `^G`. fzf's convention
  is that an empty query matches everything.
- **Wide-terminal bleed-through.** With no results, only the left pane
  is drawn; the right half of the modal area is never cleared, so
  stale transcript text shows through beside the search box.
- **No ages.** The classic picker shows "3d"; the grep rows do not, so
  two similar-titled sessions are indistinguishable.

## Requirements

- An empty query lists root sessions newest-first: title, age, and the
  session's last words as the excerpt; the preview shows the tail in
  context. Typing switches to content matches; erasing back to empty
  returns to the listing. Enter resumes either way.
- The listing streams in through the same channel/generation machinery
  as a search, and excludes the current session, like the picker.
- The preview frame is drawn whenever the terminal is wide, selection
  or not.
- Every row carries a right-aligned age.

## Acceptance Criteria

- Tests pin the listing walk (newest first, tail excerpt, current
  session excluded), the age on rows, and the always-drawn preview
  frame.
- The full suite passes.

## Milestone

11 — Beyond the terminal
