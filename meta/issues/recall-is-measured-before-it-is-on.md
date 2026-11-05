# Recall is measured before it is on

## Summary

The October labels (33 recalls, 65 notes) were not kept. Build a new
set from the session logs: real root prompts with the note store as it
stood, labelled by a strong model that reads the full note bodies and
what the session then did, with a sample checked by the user.

## Requirements

- A script that collects the cases, labels them, and runs the selector
  against the labels: precision, recall, and how often it says nothing
  when nothing applies.
- The labelled set is kept in the repo so the measurement can be rerun.

## Acceptance Criteria

- Precision clearly above the October 23%, reported in DEVLOG, before
  `memory_recall` defaults to on.
