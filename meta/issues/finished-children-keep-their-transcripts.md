# Finished children keep their transcripts

## Summary

Every `task` row folds its child's entire timeline into
`Line_::Tool.child_lines` — full assistant text via an unbounded
`append_text_delta`, one tool row per child call, recursively for
grandchildren (transcript.rs:59, 1223-1315; restore nests to depth 8
at the 256 KiB result cap). Collapsing the row only stops *rendering*
them (transcript.rs:1668-1671); nothing ever squashes the data. A
delegation-heavy day retains 10-50 MB of finished children the user
will never expand again. The bounded preview the collapsed row shows
(`agent_live_preview`, transcript.rs:1734) is what the row actually
needs.

Fix shape: on the child's `TurnDone`, squash `child_lines` to the
preview (or a capped tail), and rebuild from the store on demand if
the user expands — the log is the durable copy, not the Vec.

Size: M. Source: sweep 2026-08-31, responsiveness & memory.

## Outcome (2026-08-31)

A child's TurnDone squashes its folded timeline to
`SQUASHED_CHILD_HEAD` + marker + `SQUASHED_CHILD_TAIL` (8/24),
folding *around* live anchor rows — a background grandchild's row is
where its future events attach, and review caught that cutting one
strands its events in the retry queue forever. Restore applies the
same digest before recursing, so grandchildren of discarded rows are
never loaded. Chosen scope: inline expansion of an old delegation
shows the digest with a pointer to the focus view (which replays the
full timeline from the store) rather than rebuilding in place — the
store round-trip on expand can come later if the digest feels thin.
Deferred, narrow: a late ToolFinished whose row was folded while a
stale same-id row survives in the head is dropped silently; marker
counts drift cosmetically on re-squash.
