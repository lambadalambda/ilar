# A compacted child still reports

## Summary

Observed 2026-08-29, repeatedly: a long-running research child
finishes with a full final report (13.7K chars in its log), but its
completion notification says "(finished with no text)"; the parent
then resumes it to ask for the report and gets it fine. The dance
costs a whole extra child turn every time.

Cause: the child's turn crossed the compaction threshold mid-run.
The compaction cut hides everything before it from the loaded
window — including the task prompt, the session's only user
message. `final_assistant_text` anchors on the last UserMessage in
the window, finds none, and returns None — even though the final
assistant message with the report sits right after the cut. The
re-ask works because it appends a fresh user message, restoring an
anchor.

## Requirements

- The anchor is the last user message OR the last compaction cut,
  whichever is later: a compaction summary stands in for the prompt
  it replaced, so assistant text after it is the turn's answer.
- Applies everywhere the helper feeds: task completion
  notifications, propagated grandparent hops, the tasks tool's
  "last:" snippet.
- A test: a child whose prompt was compacted away mid-turn still
  reports its final text, not "(finished with no text)".

## Milestone

13 — Guard rails

## Outcome

`final_assistant_text` anchors on the last user message OR the
last compaction cut, whichever is later — the summary stands in
for the prompt it replaced. Every consumer (task completions,
propagated hops, the tasks tool's snippet) goes through the one
helper. Pinned by a red-first test reproducing the exact window
the live session showed: prompt compacted away, 13KB report after
the cut, previously "(finished with no text)".
