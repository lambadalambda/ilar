# A parked question pays before it fails

## Summary

`Session::append` now refuses an event written between a tool call and
its result, which is what keeps a log loadable. Two callers make a
provider call *before* the append that will be refused:

- `compaction::compact_session` summarizes, then appends `Compaction`.
- `topic::title_session` asks for a title, then appends `Topic`.

A session parked on a question has an unanswered call, so both pay for
a request and then fail on the write. Compaction surfaces the error to
the user after the wait; titling swallows it and the session stays
untitled.

## Requirements

- Both bail before the provider call when the session cannot take the
  append.

## Acceptance Criteria

- A test parks a session on a question, calls `compact_session`, and
  asserts no provider request was made and the error names the reason.
- Titling does the same and stays silent, as it does today.

## Notes

- Found reviewing the append guard, 2026-09-20.
- Size: S. `session.pending_question().is_some()` is the predicate at
  both sites; after `SessionWriter::load` it is equivalent to "has
  unanswered calls", because load answers every other one.
- The failure is already safe — nothing is written and no log is
  damaged. This is about not spending a request to learn it.
