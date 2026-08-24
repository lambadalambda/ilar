# Pin user requests through compaction

## Summary

A mid-turn compaction can summarize away the request being served. Real
case, session `9860bd12`: the opening message — "Can we do the
necessary changes to firehose to support bundled payments?" plus the
two PR links that define what bundled payments *are* — sits at event 2.
The `RecentSteps` cut landed at 245, so those 167 characters went into
the summarizer along with 771k characters of transcript, and the 2386-
character summary that came back does not mention the request or either
link. 446k characters of tool output were kept verbatim; the ask was
not.

The run still finished, because the summary's "Remaining:" list and two
later user messages survived inside the recency window. It knew what to
do next; it had lost why.

User messages are the cheapest and most load-bearing tokens in a
transcript — all three in that session together are under 400
characters. Their survival should not depend on a summarizer's
judgement.

## Requirements

- The stored compaction summary carries the verbatim user messages from
  the region it summarized, ahead of the model's prose.
- Bounded: a total character budget for the pinned block, keeping the
  first message of the summarized region (the objective) and then the
  most recent ones, with a note when older ones are dropped.
- Applies to every cut policy; `RecentSteps` is where it is
  load-bearing, but a `TurnBoundary` compaction of a long
  multi-request session loses the same way.
- No transcript schema change: pinning happens in the summary text, so
  replay and the `<compaction-summary>` fold are untouched.

## Acceptance Criteria

- A test drives a compaction whose cut is past the opening request and
  asserts that request appears verbatim in the summary the session
  stores.
- A test pins the budget: many long requests are truncated to the cap,
  the first one survives, and the drop is stated.
- The full suite passes.

## Notes

- Pairs with [reframe the summarizer prompt](reframe-the-summarizer-prompt.md):
  this issue guarantees the objective survives; that one improves
  everything around it.

## Milestone

10 — Everyday polish
