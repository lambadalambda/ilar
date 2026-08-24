# Find a session by something said in the middle of it

## Summary

The session picker filters a list of sessions by their title. That
only helps when you remember how a session *opened*. Usually you
remember a fragment from the middle — an error string, a file path, a
phrase the model used — and there is no way to search for it.

The model is the two-pane fuzzy grep: a filtered list of matching
lines on the left, each labelled with the session it came from, and
the full surrounding context of the highlighted match on the right.
Picking a match resumes that session.

## Requirements

- Search runs over the *content* of every session, not just titles:
  user messages and assistant text at minimum.
- Left pane: one row per match, showing the session's topic (falling
  back to its opening message) and the matching line with the match
  highlighted. Rows are grouped or ordered so several matches in one
  session read as belonging together.
- Right pane: the matched line in its surrounding context, with the
  match highlighted, so a row can be judged without opening it.
- Navigation moves the selection and updates the preview; Enter
  resumes the session at that point.
- Typing filters live. Search stays responsive with hundreds of
  sessions and tens of megabytes of transcript: results stream in
  rather than blocking on a full scan, and a new keystroke abandons
  the previous scan.
- Narrow terminals fall back to the list alone rather than two
  unusable columns.

## Acceptance Criteria

- Tests pin matching over session content: a phrase only present in
  the middle of a session finds it; sessions are identified by topic
  when they have one and by opening message when they do not.
- A test pins the context window around a match, including near the
  first and last line of a session.
- A test pins that the preview follows the selection.
- The full suite passes.

## Outcome

Landed in three commits: the store walk (b93154c), entries handed to
the search callback so previews cost no second read (592ff9e), and the
modal itself.

`recall::search_sessions` walks the listing newest-first, reads each
root session's full audit log — compacted material included — and
emits per-session bounded hits through a callback whose return value
abandons the walk. The TUI runs it on a blocking task, streams rows
through a channel stamped with a query generation, and drops stale
generations; every keystroke cancels the old scan and starts a new
one. Hits are capped at 5 per session and 200 overall.

Reached with `Ctrl-G` from the session picker; `Esc` returns there.
Both decisions the issue left open were settled by use: scanning is
live over the JSONL (no index until it measurably hurts), and Enter
resumes at the tail — jumping to the match is the 1% case, and rewind
already exists for it.

Smoke-tested live in tmux over copied real sessions: two panes,
preview follows selection, resume lands in the right session.

## Notes

- Existing pieces: `Ctrl-F` already searches the *current* transcript
  and highlights matches, the session picker already lists sessions
  and resumes them, and `ListNav`/`list_window`/`edit_query` already
  handle the interaction. This is a new modal that borrows all three,
  not a rewrite of any.

## Milestone

11 — Beyond the terminal
